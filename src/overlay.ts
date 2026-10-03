import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

type UiState = {
  phase: string;
  recognitionPhase: string;
  status: string;
  transcript: string;
  needsCopyPrompt: boolean;
  micNotice?: string;
  micNoticeSeq?: number;
  sessionPinned?: boolean;
};

type DeliveryResult = {
  pasted: boolean;
  copied: boolean;
  message: string;
};

const capsule = () => document.querySelector<HTMLElement>("#capsule");
const statusEl = () => document.querySelector<HTMLElement>("#overlay-status");
const textEl = () => document.querySelector<HTMLElement>("#overlay-text");
const pinBtn = () => document.querySelector<HTMLButtonElement>("#pin-btn");
const copyBtn = () => document.querySelector<HTMLButtonElement>("#copy-btn");
const retryBtn = () => document.querySelector<HTMLButtonElement>("#retry-btn");

/**
 * Audio meter (3 monochrome micro-bars) indicating microphone activity.
 * Attack is instant, decay is gently damped to prevent visual jitter.
 */
let targetLevel = 0;
let renderedLevel = 0;
let meterRaf: number | null = null;
let meterBarsCached: HTMLElement[] | null = null;

function getBars(): HTMLElement[] {
  if (!meterBarsCached || meterBarsCached.length === 0) {
    meterBarsCached = Array.from(document.querySelectorAll<HTMLElement>(".meter-bar"));
  }
  return meterBarsCached;
}

function updateMeterFrame() {
  meterRaf = null;

  if (targetLevel > renderedLevel) {
    // Attack: instant jump so the user sees sound right away
    renderedLevel += (targetLevel - renderedLevel) * 0.75;
  } else {
    // Decay: smooth organic fall-off (prevents harsh high-frequency flickering)
    renderedLevel += (targetLevel - renderedLevel) * 0.18;
    if (renderedLevel < 0.005) {
      renderedLevel = 0;
    }
  }

  const bars = getBars();
  if (bars.length === 3) {
    // Waveform silhouette: 3 bars form a natural hill shape even at rest
    const base1 = 4.5;
    const base2 = 7.0;
    const base3 = 5.5;

    // Perceptual curve: square-root curve boosts quiet/normal speech into prominent motion.
    const visual = Math.pow(Math.max(0, Math.min(1, renderedLevel)), 0.55);

    const h1 = base1 + visual * 7.5;
    const h2 = base2 + visual * 10.0;
    const h3 = base3 + visual * 7.0;

    // Monochrome contrast: bright pure white when speaking, clean solid muted gray when silent
    const alpha = visual > 0.03
      ? Math.min(0.96, 0.38 + visual * 0.58)
      : 0.38;
    const color = `rgba(255, 255, 255, ${alpha.toFixed(2)})`;

    bars[0].style.height = `${h1.toFixed(1)}px`;
    bars[0].style.backgroundColor = color;

    bars[1].style.height = `${h2.toFixed(1)}px`;
    bars[1].style.backgroundColor = color;

    bars[2].style.height = `${h3.toFixed(1)}px`;
    bars[2].style.backgroundColor = color;
  }

  if (renderedLevel > 0 || targetLevel > 0) {
    meterRaf = window.requestAnimationFrame(updateMeterFrame);
  }
}

function setAudioLevel(level: number) {
  targetLevel = Math.max(0, Math.min(1, level));
  if (meterRaf === null) {
    meterRaf = window.requestAnimationFrame(updateMeterFrame);
  }
}

function resetAudioMeter() {
  targetLevel = 0;
  if (meterRaf === null) {
    meterRaf = window.requestAnimationFrame(updateMeterFrame);
  }
}

/**
 * Result mode: dictation ended but insertion was NOT detected.
 * The capsule comes back with a manual "copy" button so the user can
 * re-copy the text themselves and nothing gets silently lost.
 */
let resultActive = false;
let resultStatusText = "未检测到插入";
let resultDismissTimer: number | undefined;
let copyDismissTimer: number | undefined;

const RESULT_AUTO_DISMISS_MS = 12000;
const COPY_FEEDBACK_MS = 900;

function paintResultChrome() {
  capsule()?.classList.add("result");
  const status = statusEl();
  if (status) status.textContent = resultStatusText;
  retryBtn()?.classList.remove("hidden");
  copyBtn()?.classList.remove("hidden");
}

function enterResultMode() {
  resultActive = true;
  resultStatusText = "未检测到插入";
  paintResultChrome();
  if (resultDismissTimer) window.clearTimeout(resultDismissTimer);
  resultDismissTimer = window.setTimeout(() => {
    void dismissResult();
  }, RESULT_AUTO_DISMISS_MS);
}

function exitResultMode() {
  resultActive = false;
  if (resultDismissTimer) {
    window.clearTimeout(resultDismissTimer);
    resultDismissTimer = undefined;
  }
  if (copyDismissTimer) {
    window.clearTimeout(copyDismissTimer);
    copyDismissTimer = undefined;
  }
  capsule()?.classList.remove("result");
  retryBtn()?.classList.add("hidden");
  copyBtn()?.classList.add("hidden");
}

async function dismissResult() {
  exitResultMode();
  try {
    await invoke("dismiss_overlay");
  } catch {
    // Ignore hide failures; the capsule auto-hides on the next session anyway.
  }
}

/**
 * Transient microphone notices (fallback to default mic, mid-session
 * disconnect/restore). They briefly use the small status line so live
 * transcript content is never hidden.
 */
const MIC_NOTICE_MS = 2600;
let lastState: UiState | null = null;
let lastNoticeSeq = 0;
let noticeSeqInitialized = false;
let activeNotice: string | null = null;
let noticeTimer: number | undefined;

function clearNotice() {
  activeNotice = null;
  if (noticeTimer) {
    window.clearTimeout(noticeTimer);
    noticeTimer = undefined;
  }
  capsule()?.classList.remove("notice");
  if (lastState) renderStatus(lastState);
}

function showNotice(text: string) {
  activeNotice = text;
  capsule()?.classList.add("notice");
  if (lastState) renderStatus(lastState);
  if (noticeTimer) window.clearTimeout(noticeTimer);
  noticeTimer = window.setTimeout(() => {
    clearNotice();
    if (lastState) renderPreview(lastState);
  }, MIC_NOTICE_MS);
}

function renderPreview(state: UiState) {
  if (state.phase === "error") {
    // The status line carries the real error message; keep the preview quiet.
    const text = textEl();
    if (text) {
      text.textContent = "本地录音发生错误";
      text.classList.add("empty");
    }
    return;
  }
  setPreviewText(state.transcript || "");
}

function renderStatus(state: UiState) {
  const status = statusEl();
  if (!status) return;
  if (activeNotice) status.textContent = activeNotice;
  else if (state.sessionPinned) {
    if (state.phase === "finalizing") status.textContent = "正在收入待办 📌";
    else status.textContent = "正在听写 · 📌 待办模式";
  }
  else if (state.phase === "starting") status.textContent = "启动录音中";
  else if (state.phase === "connecting") status.textContent = "连接中";
  else if (state.phase === "recording") {
    if (state.recognitionPhase === "connecting") status.textContent = "录音中 · 连接中";
    else if (state.recognitionPhase === "streaming") status.textContent = "正在听写";
    else status.textContent = "本地录音中";
  }
  else if (state.phase === "finalizing") status.textContent = "收尾中";
  else if (state.phase === "error") status.textContent = state.status || "出错了";
  else status.textContent = "待命";
}

async function copyResult() {
  try {
    await invoke("copy_last_transcript");
    resultStatusText = "已复制到剪贴板";
    if (resultDismissTimer) {
      window.clearTimeout(resultDismissTimer);
      resultDismissTimer = undefined;
    }
    const status = statusEl();
    if (status) status.textContent = resultStatusText;
    if (copyDismissTimer) window.clearTimeout(copyDismissTimer);
    copyDismissTimer = window.setTimeout(() => {
      void dismissResult();
    }, COPY_FEEDBACK_MS);
  } catch {
    resultStatusText = "复制失败，请重试";
    const status = statusEl();
    if (status) status.textContent = resultStatusText;
  }
}

async function retryResult() {
  const retry = retryBtn();
  const copy = copyBtn();
  if (retry) retry.disabled = true;
  if (copy) copy.disabled = true;
  resultStatusText = "正在重试";
  const status = statusEl();
  if (status) status.textContent = resultStatusText;

  try {
    const delivery = await invoke<DeliveryResult>("retry_last_transcript");
    if (delivery.pasted) {
      exitResultMode();
      return;
    }
    resultStatusText = "仍未插入 · 剪贴板未改变";
    paintResultChrome();
  } catch {
    resultStatusText = "重试失败 · 剪贴板未改变";
    paintResultChrome();
  } finally {
    if (retry) retry.disabled = false;
    if (copy) copy.disabled = false;
  }
}

/**
 * One-line preview that always shows the newest content.
 * We don't rely on CSS ellipsis/direction hacks, because those keep showing the start.
 * Instead, measure and keep only the visible suffix.
 */
function setPreviewText(value: string) {
  const text = textEl();
  if (!text) return;

  const content = (value || "").replace(/\s+/g, " ").trim();
  text.classList.toggle("empty", !content);

  if (!content) {
    text.textContent = "等待语音…";
    return;
  }

  // Fast path: if short enough, show full text.
  text.textContent = content;
  if (text.scrollWidth <= text.clientWidth + 1) {
    return;
  }

  // Keep the newest end. Prefix with ellipsis only when truncated.
  const available = Math.max(8, text.clientWidth);
  const canvas = document.createElement("canvas");
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    // Fallback: keep last N chars.
    const tail = content.slice(-24);
    text.textContent = `…${tail}`;
    return;
  }

  const style = window.getComputedStyle(text);
  ctx.font = `${style.fontWeight} ${style.fontSize} ${style.fontFamily}`;

  let low = 0;
  let high = content.length;
  let best = content;

  while (low <= high) {
    const mid = Math.floor((low + high) / 2);
    const candidate = content.slice(content.length - mid);
    const labeled = `…${candidate}`;
    const width = ctx.measureText(labeled).width;
    if (width <= available) {
      best = labeled;
      low = mid + 1;
    } else {
      high = mid - 1;
    }
  }

  text.textContent = best;
}

function applyState(state: UiState) {
  lastState = state;

  // A new session supersedes any pending result card.
  if (resultActive && state.phase !== "idle") {
    exitResultMode();
  }

  // Mic notices only matter while a session is live; drop stale ones when a
  // session winds down or a new one begins.
  if (
    activeNotice &&
    (state.phase === "starting" ||
      state.phase === "connecting" ||
      state.phase === "finalizing" ||
      state.phase === "idle" ||
      state.phase === "error")
  ) {
    clearNotice();
  }

  if (!noticeSeqInitialized) {
    lastNoticeSeq = state.micNoticeSeq ?? 0;
    noticeSeqInitialized = true;
  } else if (
    state.micNotice &&
    (state.micNoticeSeq ?? 0) !== lastNoticeSeq
  ) {
    lastNoticeSeq = state.micNoticeSeq ?? 0;
    showNotice(state.micNotice);
  }

  const root = capsule();
  if (root) {
    root.classList.remove("recording", "finalizing", "error", "result");
    if (
      state.phase === "starting" ||
      state.phase === "recording" ||
      state.phase === "connecting"
    ) {
      root.classList.add("recording");
    } else if (state.phase === "finalizing") {
      root.classList.add("finalizing");
    } else if (state.phase === "error") {
      root.classList.add("error");
    }
  }

  renderStatus(state);

  pinBtn()?.classList.toggle("active", Boolean(state.sessionPinned));

  renderPreview(state);

  if (
    state.phase !== "starting" &&
    state.phase !== "recording" &&
    state.phase !== "connecting"
  ) {
    resetAudioMeter();
  }

  if (
    state.phase === "idle" &&
    state.needsCopyPrompt &&
    (state.transcript || "").trim()
  ) {
    if (!resultActive) {
      enterResultMode();
    } else {
      paintResultChrome();
    }
  } else if (resultActive && state.phase === "idle" && !state.needsCopyPrompt) {
    exitResultMode();
  } else if (resultActive && state.phase === "idle") {
    paintResultChrome();
  }
}

async function refresh(): Promise<UiState | null> {
  try {
    const state = await invoke<UiState>("get_state");
    applyState(state);
    return state;
  } catch {
    return null;
  }
}

async function persistPosition() {
  try {
    const win = getCurrentWindow();
    const pos = await win.outerPosition();
    const scale = await win.scaleFactor();
    await invoke("save_overlay_position", {
      x: pos.x / scale,
      y: pos.y / scale,
    });
  } catch {
    // Ignore position persistence failures.
  }
}

window.addEventListener("DOMContentLoaded", async () => {
  document.querySelector("#cancel-btn")?.addEventListener("click", (e) => {
    e.stopPropagation();
    if (resultActive) {
      void dismissResult();
    } else {
      void invoke("cancel_dictation");
    }
  });
  document.querySelector("#confirm-btn")?.addEventListener("click", (e) => {
    e.stopPropagation();
    void invoke("toggle_dictation");
  });
  pinBtn()?.addEventListener("click", async (e) => {
    e.stopPropagation();
    try {
      const isPinned = await invoke<boolean>("toggle_session_pin");
      if (lastState) {
        lastState.sessionPinned = isPinned;
      }
      pinBtn()?.classList.toggle("active", isPinned);
      if (lastState) {
        renderStatus(lastState);
      } else {
        const status = statusEl();
        if (status) status.textContent = isPinned ? "正在听写 · 📌 待办模式" : "正在听写";
      }
    } catch (err) {
      console.error("toggle session pin failed", err);
    }
  });
  copyBtn()?.addEventListener("click", (e) => {
    e.stopPropagation();
    void copyResult();
  });
  retryBtn()?.addEventListener("click", (e) => {
    e.stopPropagation();
    void retryResult();
  });

  // Drag the capsule body itself.
  const dragTargets = [
    capsule(),
    document.querySelector("#drag-region"),
    statusEl(),
    textEl(),
  ].filter(Boolean) as HTMLElement[];

  for (const el of dragTargets) {
    el.addEventListener("mousedown", async (e) => {
      const target = e.target as HTMLElement | null;
      if (target?.closest("button")) return;
      e.preventDefault();
      try {
        await invoke("start_overlay_drag");
      } catch {
        try {
          await getCurrentWindow().startDragging();
        } catch {
          // no-op
        }
      }
    });
  }

  window.addEventListener("mouseup", () => {
    void persistPosition();
  });

  window.addEventListener("resize", () => {
    // Recompute visible suffix when width changes.
    const current = textEl()?.textContent || "";
    if (current && current !== "等待语音…") {
      // best effort: re-fetch full state
      void refresh();
    }
  });

  await refresh();
  // Restore the result card if the webview reloaded while it was pending.
  const initial = await invoke<UiState>("get_state").catch(() => null);
  if (initial?.needsCopyPrompt && (initial.transcript || "").trim()) {
    enterResultMode();
  }

  await listen<UiState>("jackvoice://state", (event) => {
    applyState(event.payload);
  });

  await listen<number>("jackvoice://level", (event) => {
    setAudioLevel(event.payload || 0);
  });

  await listen<DeliveryResult>("jackvoice://delivery", async (event) => {
    const delivery = event.payload;
    const state = await invoke<UiState>("get_state").catch(() => null);
    if (state) applyState(state);
    if (!delivery.pasted && (state?.transcript || "").trim()) {
      enterResultMode();
    } else if (delivery.pasted) {
      exitResultMode();
    }
  });
});
