import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { inputSignalNotice, type InputSignal } from "./input-signal";

type UiState = {
  phase: string;
  recognitionPhase: string;
  status: string;
  transcript: string;
  needsCopyPrompt: boolean;
  micNotice?: string;
  micNoticeSeq?: number;
  sessionPinned?: boolean;
  /** 本轮开始录音的时刻（Unix 毫秒）；不在录音时为 0。 */
  recordingStartedAtMs?: number;
  /** 长时间无语音、即将自动结束的时刻（Unix 毫秒）；没有提醒时为 0。 */
  silenceStopAtMs?: number;
  /** 上一轮自动结束的原因："silence" / "durationLimit"；手动结束为空。 */
  autoStopReason?: string;
  inputSignal?: InputSignal;
  usingInputDeviceFallback?: boolean;
  activeInputDeviceName?: string;
};

type DeliveryResult = {
  pasted: boolean;
  copied: boolean;
  message: string;
};

type OverlayRecenteredPayload = {
  flash: boolean;
};

const capsule = () => document.querySelector<HTMLElement>("#capsule");
const statusEl = () => document.querySelector<HTMLElement>("#overlay-status");
const textEl = () => document.querySelector<HTMLElement>("#overlay-text");
const pinBtn = () => document.querySelector<HTMLButtonElement>("#pin-btn");
const copyBtn = () => document.querySelector<HTMLButtonElement>("#copy-btn");
const retryBtn = () => document.querySelector<HTMLButtonElement>("#retry-btn");
const elapsedEl = () => document.querySelector<HTMLElement>("#overlay-elapsed");
const recenterBannerEl = () => document.querySelector<HTMLElement>("#recenter-banner");

let recenterFlashActive = false;

function triggerRecenterPulse() {
  const root = capsule();
  if (!root) return;
  root.classList.remove("recenter-pulse");
  // 强制重排，保证连续双击时每次都能重播边框微光脉冲。
  void root.offsetWidth;
  root.classList.add("recenter-pulse");
}

function enterRecenterFlash() {
  recenterFlashActive = true;
  const root = capsule();
  root?.classList.remove("recenter-fading");
  root?.classList.add("recenter-flash");
  recenterBannerEl()?.classList.remove("hidden");
  triggerRecenterPulse();
}

function beginRecenterFlashFade() {
  if (!recenterFlashActive) return;
  capsule()?.classList.add("recenter-fading");
}

function exitRecenterFlash() {
  recenterFlashActive = false;
  const root = capsule();
  root?.classList.remove("recenter-flash", "recenter-fading", "recenter-pulse");
  recenterBannerEl()?.classList.add("hidden");
}

function isLive(state: UiState) {
  return (
    state.phase === "starting" ||
    state.phase === "connecting" ||
    state.phase === "recording"
  );
}

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
 * Result mode: dictation ended but insertion was NOT detected (or stopped
 * automatically). The capsule stays visible with retry and copy actions
 * until the user clicks Copy, retries successfully, closes it, or starts
 * a new dictation session.
 */
let resultActive = false;
/** 这张结果卡片来自自动结束：「重试」改叫「插入」。 */
let resultAuto = false;
let resultStatusText = "未检测到插入";
let copyDismissTimer: number | undefined;

const COPY_FEEDBACK_MS = 900;

function paintResultChrome() {
  capsule()?.classList.add("result");
  const status = statusEl();
  if (status) status.textContent = resultStatusText;
  retryBtn()?.classList.remove("hidden");
  copyBtn()?.classList.remove("hidden");
}

function autoStopText(reason: string | undefined): string | null {
  if (reason === "silence") return "长时间无语音 · 已自动结束";
  if (reason === "durationLimit") return "已录满 30 分钟 · 已自动结束";
  return null;
}

function enterResultMode(state?: UiState | null) {
  resultActive = true;
  const autoText = autoStopText(state?.autoStopReason);
  resultAuto = autoText !== null;
  resultStatusText = autoText ?? "未检测到插入";
  const retry = retryBtn();
  const copy = copyBtn();
  if (retry) {
    retry.disabled = false;
    retry.textContent = resultAuto ? "插入" : "重试";
    retry.title = resultAuto
      ? "插入，保持剪贴板不变"
      : "重新尝试插入，保持剪贴板不变";
  }
  if (copy) {
    copy.disabled = false;
  }
  paintResultChrome();
  // 切入结果模式后右侧按钮区宽度发生变化，重新测量单行截断与溢出状态。
  const effective = state ?? lastState;
  if (effective) {
    renderPreview(effective);
    syncExpansion(effective);
    window.requestAnimationFrame(() => {
      if (resultActive && lastState) {
        renderPreview(lastState);
        syncExpansion(lastState);
      }
    });
  }
}

function exitResultMode() {
  resultActive = false;
  resultAuto = false;
  if (copyDismissTimer) {
    window.clearTimeout(copyDismissTimer);
    copyDismissTimer = undefined;
  }
  const retry = retryBtn();
  const copy = copyBtn();
  if (retry) retry.disabled = false;
  if (copy) copy.disabled = false;
  capsule()?.classList.remove("result");
  retry?.classList.add("hidden");
  copy?.classList.add("hidden");
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
    previewContent = "";
    setOverflowing(false);
    return;
  }
  setPreviewText(state.transcript || "");
}

function renderStatus(state: UiState) {
  const status = statusEl();
  if (!status) return;
  const silenceText = silenceCountdownText(state);
  const inputNotice = inputSignalNotice(state.phase, state.inputSignal);
  capsule()?.classList.toggle("input-warning", inputNotice !== null);
  status.title = state.inputSignal === "silent" && inputNotice
    ? "检查麦克风连接或静音开关" : "";
  capsule()?.classList.toggle("silence-warn", silenceText !== null);
  if (inputNotice) status.textContent = inputNotice;
  else if (activeNotice) status.textContent = activeNotice;
  else if (silenceText) status.textContent = silenceText;
  else if (state.phase === "recording" && state.usingInputDeviceFallback && state.activeInputDeviceName) {
    status.textContent = `录音中 · ${state.activeInputDeviceName}`;
  }
  else if (state.sessionPinned) {
    if (state.phase === "finalizing") status.textContent = "正在收入待办…";
    else status.textContent = "正在听写 · 待办事项";
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

/** 长时间无语音时的倒计时文案；没在提醒时返回 null。 */
function silenceCountdownText(state: UiState): string | null {
  if (!isLive(state) || !state.silenceStopAtMs) return null;
  const left = Math.max(0, Math.ceil((state.silenceStopAtMs - Date.now()) / 1000));
  return `无语音 · ${left} 秒后结束`;
}

/**
 * 已录时长与长时提醒。录满一分钟才显示，十分钟起转为暖色并轻闪一次边框。
 * 状态事件在录音时很密，但不能指望它驱动秒表：用一个独立的 1s 节拍。
 */
const ELAPSED_VISIBLE_AFTER_MS = 60_000;
const LONG_TAKE_AFTER_MS = 10 * 60_000;
let clockTimer: number | undefined;
let longPulseFor = 0;

function formatElapsed(ms: number): string {
  const total = Math.floor(ms / 1000);
  const minutes = Math.floor(total / 60);
  const seconds = total % 60;
  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
}

function stopClock() {
  if (clockTimer) {
    window.clearInterval(clockTimer);
    clockTimer = undefined;
  }
}

function renderClock() {
  const el = elapsedEl();
  const root = capsule();
  const state = lastState;
  const startedAt = state?.recordingStartedAtMs ?? 0;
  if (!state || !isLive(state) || !startedAt) {
    el?.classList.add("hidden");
    root?.classList.remove("long-take", "long-pulse", "silence-warn");
    stopClock();
    return;
  }
  if (!clockTimer) {
    clockTimer = window.setInterval(() => {
      renderClock();
      // 只在录音中刷新倒计时；收尾后的结果卡片有自己的状态文案，不能被盖掉。
      if (lastState && isLive(lastState)) renderStatus(lastState);
    }, 1000);
  }

  const elapsed = Math.max(0, Date.now() - startedAt);
  if (el) {
    el.classList.toggle("hidden", elapsed < ELAPSED_VISIBLE_AFTER_MS);
    el.textContent = formatElapsed(elapsed);
  }
  const longTake = elapsed >= LONG_TAKE_AFTER_MS;
  root?.classList.toggle("long-take", longTake);
  if (longTake && longPulseFor !== startedAt) {
    longPulseFor = startedAt;
    root?.classList.add("long-pulse");
  }
}

async function copyResult() {
  const retry = retryBtn();
  const copy = copyBtn();
  if (retry) retry.disabled = true;
  if (copy) copy.disabled = true;
  try {
    await invoke("copy_last_transcript");
    resultStatusText = "已复制到剪贴板";
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
    if (retry) retry.disabled = false;
    if (copy) copy.disabled = false;
  }
}

async function retryResult() {
  const retry = retryBtn();
  const copy = copyBtn();
  if (retry) retry.disabled = true;
  if (copy) copy.disabled = true;
  resultStatusText = resultAuto ? "正在插入" : "正在重试";
  const status = statusEl();
  if (status) status.textContent = resultStatusText;

  try {
    const delivery = await invoke<DeliveryResult>("retry_last_transcript");
    if (delivery.pasted) {
      exitResultMode();
      return;
    }
    resultStatusText = resultAuto ? "未能插入 · 剪贴板未改变" : "仍未插入 · 剪贴板未改变";
    paintResultChrome();
  } catch {
    resultStatusText = resultAuto ? "插入失败 · 剪贴板未改变" : "重试失败 · 剪贴板未改变";
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
  previewContent = content;
  renderSheet();

  if (!content) {
    text.textContent = "等待语音…";
    setOverflowing(false);
    return;
  }

  // Fast path: if short enough, show full text.
  text.textContent = content;
  if (text.scrollWidth <= text.clientWidth + 1) {
    setOverflowing(false);
    return;
  }
  setOverflowing(true);

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

/**
 * 长听写的全文卡片。单行放不下时状态行后面出现展开箭头；展开后胶囊长成
 * 卡片，控制条钉在原处不动。展开与否记成偏好：上次展开过，下次一溢出就
 * 自动展开；短句永远是单行胶囊。
 */
const EXPAND_PREF_KEY = "jackvoice.overlay.expanded";
const COLLAPSED_HEIGHT = 46;
const SHEET_LINE_HEIGHT = 20;
const SHEET_MAX_LINES = 7;
/** 卡片上下内边距之和，见 overlay.css 的 `.sheet`。 */
const SHEET_CHROME = 12;
/** 收起时等窗口缩回单行再撤卡片排版；等不到就按这个时间兜底。 */
const COLLAPSE_SETTLE_MS = 300;
/** 滚动发生在最近一次滚轮之后多久内，算作用户自己在翻。 */
const WHEEL_SCROLL_WINDOW_MS = 300;
const DRAG_THRESHOLD_PX = 3;

type GrowDirection = "up" | "down";
type OverlayGeometry = { grow: GrowDirection | "none"; height: number };

const sheetScrollEl = () => document.querySelector<HTMLElement>("#sheet-scroll");
const sheetFlowEl = () => document.querySelector<HTMLElement>("#sheet-flow");
const sheetSaidEl = () => document.querySelector<HTMLElement>("#sheet-said");
const sheetCurrentEl = () => document.querySelector<HTMLElement>("#sheet-current");
const expandBtn = () => document.querySelector<HTMLButtonElement>("#expand-btn");
const jumpBtn = () => document.querySelector<HTMLButtonElement>("#jump-btn");
const contentEl = () => document.querySelector<HTMLElement>("#drag-region");

let previewContent = "";
let overflowing = false;
let expanded = false;
let growDir: GrowDirection = "up";
let followLatest = true;
let lastWheelAt = 0;
let collapseTimer: number | undefined;

function readExpandPref(): boolean {
  try {
    return window.localStorage.getItem(EXPAND_PREF_KEY) === "1";
  } catch {
    return false;
  }
}

function writeExpandPref(value: boolean) {
  try {
    window.localStorage.setItem(EXPAND_PREF_KEY, value ? "1" : "0");
  } catch {
    // 记不住就每次手动展开，不影响听写。
  }
}

let expandPref = readExpandPref();

/** 窗口高度请求排队：同一时刻只发一个，期间的新高度合并成最后一次。 */
let appliedHeight = COLLAPSED_HEIGHT;
let wantedHeight = COLLAPSED_HEIGHT;
let geometryBusy = false;

async function flushHeight() {
  if (geometryBusy) return;
  geometryBusy = true;
  try {
    while (wantedHeight !== appliedHeight) {
      const target = wantedHeight;
      try {
        const geometry = await invoke<OverlayGeometry>("set_overlay_height", { height: target });
        if (geometry.grow !== "none") setGrowDirection(geometry.grow);
      } catch (err) {
        console.error("resize overlay failed", err);
      }
      appliedHeight = target;
    }
  } finally {
    geometryBusy = false;
  }
}

function requestHeight(height: number) {
  wantedHeight = Math.round(height);
  void flushHeight();
}

function setGrowDirection(dir: GrowDirection) {
  growDir = dir;
  capsule()?.classList.toggle("grow-down", dir === "down");
  paintExpandButton();
}

async function refreshGrowDirection() {
  if (expanded) return;
  try {
    const dir = await invoke<GrowDirection>("overlay_grow_direction");
    if (!expanded) setGrowDirection(dir);
  } catch {
    // 拿不到就沿用上次的方向。
  }
}

function setOverflowing(next: boolean) {
  if (next === overflowing) return;
  overflowing = next;
  if (next && !expanded) void refreshGrowDirection();
  paintExpandButton();
}

/** 箭头指向卡片会长出去的方向；展开后反过来，表示收回。 */
function paintExpandButton() {
  const available = overflowing || expanded;
  const btn = expandBtn();
  if (btn) {
    btn.classList.toggle("hidden", !available);
    btn.classList.toggle("flip", (growDir === "up") === expanded);
    btn.title = expanded ? "收起" : "展开全文";
  }
  contentEl()?.classList.toggle("expandable", available);
}

/** 最后一个句末标点之后是正在说的这句；末尾刚好落在标点上时，取上一句。 */
function splitLatestSentence(text: string): [string, string] {
  const body = text.slice(0, -1);
  let cut = -1;
  for (const match of body.matchAll(/[。！？!?；;]|\.(?=\s)/g)) {
    cut = (match.index ?? 0) + match[0].length;
  }
  return cut <= 0 ? ["", text] : [text.slice(0, cut), text.slice(cut)];
}

function sheetWindowHeight(): number {
  const flow = sheetFlowEl()?.offsetHeight ?? SHEET_LINE_HEIGHT;
  const text = Math.min(
    Math.max(flow, SHEET_LINE_HEIGHT),
    SHEET_LINE_HEIGHT * SHEET_MAX_LINES,
  );
  return COLLAPSED_HEIGHT + SHEET_CHROME + text;
}

function paintSheetClip() {
  const el = sheetScrollEl();
  el?.classList.toggle("clipped", Boolean(el && el.scrollTop > 1));
}

function scrollSheetToLatest() {
  const el = sheetScrollEl();
  if (el) el.scrollTop = el.scrollHeight;
  paintSheetClip();
}

function setFollowLatest(next: boolean) {
  followLatest = next;
  jumpBtn()?.classList.toggle("hidden", next);
}

function renderSheet() {
  if (!expanded) return;
  const [said, current] = splitLatestSentence(previewContent);
  const saidEl = sheetSaidEl();
  const currentEl = sheetCurrentEl();
  if (saidEl && saidEl.textContent !== said) saidEl.textContent = said;
  if (currentEl && currentEl.textContent !== current) currentEl.textContent = current;
  requestHeight(sheetWindowHeight());
  if (followLatest) scrollSheetToLatest();
}

function finishCollapse() {
  if (collapseTimer) {
    window.clearTimeout(collapseTimer);
    collapseTimer = undefined;
  }
  if (!expanded) capsule()?.classList.remove("expanded");
}

function setExpanded(next: boolean) {
  if (next === expanded) return;
  expanded = next;
  if (next) {
    if (collapseTimer) {
      window.clearTimeout(collapseTimer);
      collapseTimer = undefined;
    }
    setFollowLatest(true);
    capsule()?.classList.add("expanded");
    renderSheet();
  } else {
    requestHeight(COLLAPSED_HEIGHT);
    // 等窗口缩回单行再撤卡片排版，免得高窗口里先露出一截空胶囊。
    if (collapseTimer) window.clearTimeout(collapseTimer);
    collapseTimer = window.setTimeout(finishCollapse, COLLAPSE_SETTLE_MS);
  }
  paintExpandButton();
}

function toggleExpanded() {
  const next = !expanded;
  expandPref = next;
  writeExpandPref(next);
  setExpanded(next);
}

/**
 * 只有录音中、收尾中或结果卡片上才可能展开；自动展开只发生在录音中和结果
 * 卡片上，免得胶囊已经藏起来后又被后台的收尾状态撑大。
 */
function syncExpansion(state: UiState | null) {
  const live = state ? isLive(state) : false;
  const showing = live || resultActive || state?.phase === "finalizing";
  if (!overflowing || !showing) {
    if (expanded) setExpanded(false);
  } else if (!expanded && expandPref && (live || resultActive)) {
    setExpanded(true);
  }
  paintExpandButton();
}

/** 后端藏起胶囊时已经把窗口缩回单行，这里只撤掉排版。 */
function dropExpandedLayout() {
  expanded = false;
  appliedHeight = COLLAPSED_HEIGHT;
  wantedHeight = COLLAPSED_HEIGHT;
  finishCollapse();
  capsule()?.classList.remove("expanded", "grow-down");
  growDir = "up";
  setFollowLatest(true);
  paintExpandButton();
}

function applyState(state: UiState) {
  lastState = state;

  // 新一轮听写 / 收尾 / 错误优先于短暂的居中闪现。
  if (recenterFlashActive && state.phase !== "idle") {
    exitRecenterFlash();
  }

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
  renderClock();

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
    if (recenterFlashActive) {
      exitRecenterFlash();
    }
    if (!resultActive) {
      enterResultMode(state);
    } else {
      paintResultChrome();
    }
  } else if (resultActive && state.phase === "idle" && !state.needsCopyPrompt) {
    exitResultMode();
  } else if (resultActive && state.phase === "idle") {
    paintResultChrome();
  }

  syncExpansion(state);
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

type OverlayCursorKind = "default" | "grab" | "grabbing" | "pointer";
type OverlayHoverEvent = {
  inside: boolean;
  x: number;
  y: number;
};

let activeCursor: OverlayCursorKind = "default";
let dragInFlight = false;
let hoveredControl: HTMLElement | null = null;
let capsuleHovered = false;
let lastHoverPoint: { x: number; y: number } | null = null;

function syncNativeCursor(next: OverlayCursorKind) {
  if (next === activeCursor) return;
  activeCursor = next;
  void invoke("set_overlay_cursor", { cursor: next }).catch(() => {
    // 同步光标失败不影响胶囊功能。
  });
}

function restoreOverlayFocus() {
  void invoke("restore_overlay_focus").catch(() => {
    // 归还焦点失败不影响听写主流程。
  });
}

function updateHoverTarget(target: HTMLElement | null) {
  const nextControl = target?.closest<HTMLButtonElement>("button:not(:disabled)") ?? null;
  if (hoveredControl !== nextControl) {
    hoveredControl?.classList.remove("hovered");
    nextControl?.classList.add("hovered");
    hoveredControl = nextControl;
  }
  const onContent = Boolean(target?.closest("#drag-region"));
  contentEl()?.classList.toggle("hovered", onContent);
  if (!dragInFlight) {
    syncNativeCursor(nextControl ? "pointer" : "grab");
  }
}

function clearHoverVisuals() {
  capsuleHovered = false;
  lastHoverPoint = null;
  capsule()?.classList.remove("hovered");
  hoveredControl?.classList.remove("hovered");
  hoveredControl = null;
  contentEl()?.classList.remove("hovered");
  if (!dragInFlight) {
    syncNativeCursor("default");
  }
}

function refreshResultTargetIfNeeded() {
  if (!resultActive) return;
  void invoke("refresh_overlay_target").catch(() => {
    // 刷新失败沿用原目标，不影响重试或复制。
  });
}

function applyNativeHover(payload: OverlayHoverEvent) {
  if (!payload.inside) {
    clearHoverVisuals();
    return;
  }
  if (!capsuleHovered) {
    capsuleHovered = true;
    refreshResultTargetIfNeeded();
  }
  capsule()?.classList.add("hovered");
  if (payload.x >= 0 && payload.y >= 0) {
    lastHoverPoint = { x: payload.x, y: payload.y };
    const hit = document.elementFromPoint(payload.x, payload.y) as HTMLElement | null;
    updateHoverTarget(hit);
  } else if (!dragInFlight) {
    syncNativeCursor("grab");
  }
}

/**
 * 胶囊自己会把 JackVoice 激活到前台，macOS 随后补发一个 Reopen（相当于
 * 点了程序坞图标）。把「刚刚是胶囊被点」这件事告诉后端，它才只吞掉紧跟着
 * 的那一次 Reopen，而不是把所有 Dock 点击都当成自己人。
 */
function noteInteraction() {
  void invoke("note_overlay_interaction").catch(() => {
    // 记录失败不影响胶囊功能。
  });
}

window.addEventListener("DOMContentLoaded", async () => {
  document.querySelector("#capsule")?.addEventListener("mousedown", noteInteraction, true);
  capsule()?.addEventListener("mouseenter", (e) => {
    if (!capsuleHovered) {
      capsuleHovered = true;
      refreshResultTargetIfNeeded();
    }
    lastHoverPoint = { x: e.clientX, y: e.clientY };
    capsule()?.classList.add("hovered");
    updateHoverTarget(e.target as HTMLElement | null);
  });
  capsule()?.addEventListener("mousemove", (e) => {
    if (!capsuleHovered) {
      capsuleHovered = true;
      capsule()?.classList.add("hovered");
    }
    lastHoverPoint = { x: e.clientX, y: e.clientY };
    updateHoverTarget(e.target as HTMLElement | null);
  });
  capsule()?.addEventListener("mouseleave", () => {
    clearHoverVisuals();
  });
  capsule()?.addEventListener("animationend", (e) => {
    if (e.animationName === "long-take-pulse") capsule()?.classList.remove("long-pulse");
    if (e.animationName === "recenter-pulse") capsule()?.classList.remove("recenter-pulse");
  });
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
        if (status) status.textContent = isPinned ? "正在听写 · 待办事项" : "正在听写";
      }
    } catch (err) {
      console.error("toggle session pin failed", err);
    } finally {
      restoreOverlayFocus();
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

  expandBtn()?.addEventListener("click", (e) => {
    e.stopPropagation();
    toggleExpanded();
    restoreOverlayFocus();
  });
  jumpBtn()?.addEventListener("click", (e) => {
    e.stopPropagation();
    setFollowLatest(true);
    const el = sheetScrollEl();
    el?.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
    restoreOverlayFocus();
  });

  // 只有用户自己滚动才改变「跟随最新」：新文字撑高、窗口变高引起的滚动不算。
  const sheetScroll = sheetScrollEl();
  sheetScroll?.addEventListener(
    "wheel",
    () => {
      lastWheelAt = Date.now();
    },
    { passive: true },
  );
  sheetScroll?.addEventListener(
    "scroll",
    () => {
      paintSheetClip();
      if (Date.now() - lastWheelAt > WHEEL_SCROLL_WINDOW_MS) return;
      const el = sheetScrollEl();
      if (!el) return;
      setFollowLatest(el.scrollHeight - el.scrollTop - el.clientHeight < 4);
    },
    { passive: true },
  );

  async function beginOverlayDrag() {
    if (dragInFlight) return;
    dragInFlight = true;
    capsule()?.classList.add("dragging");
    syncNativeCursor("grabbing");
    try {
      await invoke<boolean>("start_overlay_drag");
    } catch {
      try {
        await getCurrentWindow().startDragging();
      } catch {
        // no-op
      }
      await persistPosition();
      restoreOverlayFocus();
    } finally {
      dragInFlight = false;
      capsule()?.classList.remove("dragging");
      activeCursor = "default";
      if (capsuleHovered) {
        if (lastHoverPoint) {
          const hit = document.elementFromPoint(
            lastHoverPoint.x,
            lastHoverPoint.y,
          ) as HTMLElement | null;
          updateHoverTarget(hit);
        } else {
          syncNativeCursor("grab");
        }
      }
      // 拖到屏幕另一半后，下次展开的方向也跟着变。
      if (overflowing && !expanded) void refreshGrowDirection();
    }
  }

  // 非展开文字区按下去即刻拖拽；可展开文字栏通过 3px 阈值区分点击展开与拖动。
  let press: { x: number; y: number; onContent: boolean } | null = null;
  capsule()?.addEventListener("mousedown", (e) => {
    if (e.button !== 0) return;
    const target = e.target as HTMLElement | null;
    if (target?.closest("button")) return;
    e.preventDefault();
    const onContent = Boolean(target?.closest("#drag-region"));
    if (onContent && (overflowing || expanded)) {
      press = {
        x: e.screenX,
        y: e.screenY,
        onContent: true,
      };
      syncNativeCursor("grabbing");
      return;
    }
    void beginOverlayDrag();
  });

  window.addEventListener("mousemove", (e) => {
    if (!press || (e.buttons & 1) === 0) return;
    const moved = Math.abs(e.screenX - press.x) + Math.abs(e.screenY - press.y);
    if (moved < DRAG_THRESHOLD_PX) return;
    press = null;
    void beginOverlayDrag();
  });

  window.addEventListener("mouseup", () => {
    const clicked = press;
    press = null;
    if (clicked?.onContent && (overflowing || expanded)) {
      toggleExpanded();
      restoreOverlayFocus();
      if (!dragInFlight) syncNativeCursor(capsuleHovered ? "grab" : "default");
      return;
    }
    if (!dragInFlight) {
      void persistPosition();
      if (overflowing && !expanded) void refreshGrowDirection();
    }
  });

  let lastWidth = window.innerWidth;
  window.addEventListener("resize", () => {
    // Recompute visible suffix when width changes.
    if (window.innerWidth !== lastWidth) {
      lastWidth = window.innerWidth;
      void refresh();
    }
    if (!expanded && window.innerHeight <= COLLAPSED_HEIGHT + 2) finishCollapse();
    if (expanded && followLatest) scrollSheetToLatest();
    else paintSheetClip();
  });

  await refresh();
  // Restore the result card if the webview reloaded while it was pending.
  const initial = await invoke<UiState>("get_state").catch(() => null);
  if (initial?.needsCopyPrompt && (initial.transcript || "").trim()) {
    enterResultMode(initial);
  }

  await listen<UiState>("jackvoice://state", (event) => {
    applyState(event.payload);
  });

  await listen("jackvoice://overlay-collapsed", () => {
    dropExpandedLayout();
  });

  await listen<OverlayRecenteredPayload>("jackvoice://overlay-recentered", (event) => {
    if (event.payload?.flash) {
      enterRecenterFlash();
    } else {
      triggerRecenterPulse();
    }
  });

  await listen("jackvoice://overlay-flash-fading", () => {
    beginRecenterFlashFade();
  });

  await listen("jackvoice://overlay-flash-ended", () => {
    exitRecenterFlash();
  });

  await listen<OverlayHoverEvent>("jackvoice://overlay-hover", (event) => {
    applyNativeHover(event.payload);
  });

  await listen<number>("jackvoice://level", (event) => {
    setAudioLevel(event.payload || 0);
  });

  await listen<DeliveryResult>("jackvoice://delivery", async (event) => {
    const delivery = event.payload;
    const state = await invoke<UiState>("get_state").catch(() => null);
    if (state) applyState(state);
    // 胶囊藏不藏由后端定：插进去了它会自己收起（收尾的 report_finalize、
    // 手动重试的 retry_last_transcript 都是）。这里不能再调 dismiss_overlay ——
    // 投递事件到达时用户可能已经按下 Option+Space 开了新一轮，前端一藏，
    // 新一轮就成了「在录但看不见」。
    if (delivery.pasted) {
      exitResultMode();
    } else if (state?.phase === "idle" && (state.transcript || "").trim()) {
      enterResultMode(state);
    }
  });

  const flashActive = await invoke<boolean>("overlay_ready").catch(() => false);
  if (flashActive && (!lastState || (!isLive(lastState) && !resultActive))) {
    enterRecenterFlash();
  }
});
