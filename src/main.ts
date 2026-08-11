import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";

type InputDeviceInfo = { id: string; name: string; isDefault: boolean };

type PermissionStatus = {
  microphone: boolean;
  microphoneAuthorization: "notDetermined" | "restricted" | "denied" | "authorized";
  accessibility: boolean;
};

type UiState = {
  phase: string;
  recognitionPhase: string;
  status: string;
  transcript: string;
  hasVolcApiKey: boolean;
  volcCredentialStatus: "missing" | "configured" | "verified" | "failed" | "unavailable";
  maskedVolcApiKey: string;
  volcCredentialSource: string;
  volcCredentialWarning: string;
  volcResourceId: string;
  volcBoostingTableId: string;
  semanticPunctuationEnabled: boolean;
  semanticSmoothingEnabled: boolean;
  maxSentenceSilenceMs: number;
  inputGainDb: number;
  selectedInputDeviceId: string;
  inputDevices: InputDeviceInfo[];
  audioLevel: number;
  micTesting: boolean;
  lastDeliveryMessage: string;
  needsCopyPrompt: boolean;
  shortcut: string;
  launchAtLogin: boolean;
  muteSystemAudioDuringDictation: boolean;
  systemAudioMuteSupported: boolean;
  onboardingCompleted: boolean;
  historyTextSize: string;
};

type SaveHotwordsResult = {
  words: string[];
  synced: boolean;
  message: string;
  syncError?: string | null;
};

type ReplacementRule = {
  from: string;
  to: string;
};

type HistoryRecord = {
  id: string;
  finishedAtMs: number;
  text: string;
  durationMs: number;
  charCount: number;
  audio?: {
    fileName: string;
    mimeType: string;
    sampleRateHz: number;
    channels: number;
    bitsPerSample: number;
    durationMs: number;
    sizeBytes: number;
  };
  recognition?: {
    hotwords: string[];
    semanticPunctuationEnabled: boolean;
    semanticSmoothingEnabled: boolean;
    maxSentenceSilenceMs: number;
    inputGainDb: number;
    inputDeviceId: string;
  };
  recognitionStatus?: "completed" | "noSpeech" | "failed";
  recognitionError?: string;
  recordingError?: string;
  audioMissing?: boolean;
};

type HistoryStats = {
  sessions: number;
  totalDurationMs: number;
  totalChars: number;
};

type HistoryData = { records: HistoryRecord[]; stats: HistoryStats };

// 火山引擎热词文本规范：识别形去空格/标点，最长 32 字。
const VOLC_HOTWORD_MAX_CHARS = 32;
// 火山平台热词表上限。
const HOTWORD_LIMIT = 5000;
const WEEKDAYS = ["星期日", "星期一", "星期二", "星期三", "星期四", "星期五", "星期六"];

const $ = <T extends Element = HTMLElement>(selector: string) =>
  document.querySelector<T>(selector) as T | null;

let currentState: UiState | null = null;
let hotwords: string[] = [];
let hotwordFilter = "";
let replacements: ReplacementRule[] = [];
let replacementFilter = "";
let recordingShortcut = false;
let historyRecords: HistoryRecord[] = [];
let selectedHistoryId: string | null = null;
let historyQuery = "";
let historyPlaybackRate = 1;
let historyPlaybackLoadingId: string | null = null;
let historyPlaybackLoadGeneration = 0;
let historyPlayback: {
  recordId: string;
  audio: HTMLAudioElement;
  objectUrl: string;
} | null = null;
let toastTimer: number | undefined;
let dictationElapsedTimer: number | undefined;
let dictationRecordingStartedAt: number | null = null;
let lastDictationPhase = "idle";
let credentialEditorOpen = false;
let onboardingCredentialEditorOpen = false;
let onboardingMicVerified = false;
let onboardingMicNeedsManualRecovery = false;
let onboardingAccessibilityGranted = false;

const COPY_SUCCESS_ICON =
  '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="m5 12 4 4L19 6"/></svg>';
const COPY_ERROR_ICON =
  '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><path d="M12 7v6"/><path d="M12 17h.01"/></svg>';

function showToast(message: string, kind: "success" | "error") {
  const toast = $("#app-toast");
  if (!toast) return;

  if (toastTimer) window.clearTimeout(toastTimer);
  toast.textContent = message;
  toast.className = `app-toast ${kind}`;
  toastTimer = window.setTimeout(() => {
    toast.classList.add("hidden");
    toastTimer = undefined;
  }, 1800);
}

async function copyHistoryText(text: string, button: HTMLButtonElement) {
  const original = button.innerHTML;
  const textButton = button.classList.contains("detail-action");
  button.disabled = true;
  button.title = "正在复制";

  try {
    await writeText(text);
    button.innerHTML = textButton ? "已复制" : COPY_SUCCESS_ICON;
    button.classList.add("copied");
    button.title = "已复制";
    showToast("已复制到剪贴板", "success");
  } catch (error) {
    console.error("copy history text failed", error);
    button.innerHTML = textButton ? "复制失败" : COPY_ERROR_ICON;
    button.classList.add("copy-failed");
    button.title = "复制失败，请重试";
    showToast("复制失败，请重试", "error");
  } finally {
    button.disabled = false;
    window.setTimeout(() => {
      button.innerHTML = original;
      button.classList.remove("copied", "copy-failed");
      button.title = "复制";
    }, 1500);
  }
}

/* ---------------- 热词规范（火山引擎） ---------------- */

/** 火山热词条数上限。 */
function hotwordLimit(): number {
  return HOTWORD_LIMIT;
}

/** 按火山热词文本规范判断单条热词是否合法。 */
function isValidHotword(text: string): boolean {
  const t = text.trim();
  if (!t) return false;
  const recognition = Array.from(t)
    .filter((c) => /[\p{L}\p{N}]/u.test(c))
    .join("");
  return (
    recognition.length > 0 &&
    Array.from(recognition).length <= VOLC_HOTWORD_MAX_CHARS
  );
}

/* ---------------- 通用格式化 ---------------- */

function modLabel(token: string): string {
  switch (token) {
    case "Alt":
    case "Option":
      return "⌥";
    case "Meta":
    case "Cmd":
    case "Command":
    case "Super":
      return "⌘";
    case "Ctrl":
    case "Control":
      return "⌃";
    case "Shift":
      return "⇧";
    case "Fn":
      return "Fn";
    case "Equal":
      return "=";
    default:
      return token;
  }
}

function formatShortcut(accelerator: string): string {
  return accelerator
    .split("+")
    .map((t) => modLabel(t.trim()))
    .join(" + ");
}

function formatDuration(ms: number): string {
  const totalMin = Math.floor(ms / 60000);
  const h = Math.floor(totalMin / 60);
  const m = totalMin % 60;
  if (h <= 0 && m <= 0) return ms > 0 ? "不足 1 分" : "0 分";
  return `${h} 时 ${m} 分`;
}

function formatChars(n: number): string {
  if (n >= 10000) return `${(n / 10000).toFixed(1)}w 字`;
  return `${n} 字`;
}

function dayLabel(d: Date): string {
  return `${d.getMonth() + 1}月${d.getDate()}日 ${WEEKDAYS[d.getDay()]}`;
}

function timeLabel(d: Date): string {
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  return `${hh}:${mm}`;
}

function formatClipDuration(ms: number): string {
  const totalSeconds = Math.max(0, Math.round(ms / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = String(totalSeconds % 60).padStart(2, "0");
  return `${minutes}:${seconds}`;
}

function formatPlaybackTime(seconds: number): string {
  return formatClipDuration(Math.max(0, seconds) * 1000);
}

function formatFileSize(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${Math.max(1, Math.round(bytes / 1024))} KB`;
}

function phaseLabel(phase: string): string {
  switch (phase) {
    case "recording":
      return "正在听写";
    case "starting":
      return "正在启动录音";
    case "connecting":
      return "连接中";
    case "finalizing":
      return "收尾中";
    case "error":
      return "出错";
    default:
      return "准备就绪";
  }
}

function formatDictationElapsed(elapsedMs: number): string {
  const totalSeconds = Math.max(0, Math.floor(elapsedMs / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`;
}

function updateDictationElapsed() {
  const elapsed = $("#dictation-elapsed");
  if (!elapsed || dictationRecordingStartedAt === null) return;
  elapsed.textContent = formatDictationElapsed(Date.now() - dictationRecordingStartedAt);
}

function syncDictationElapsed(phase: string) {
  const elapsed = $("#dictation-elapsed");
  const isRecording = phase === "recording";

  if (isRecording && lastDictationPhase !== "recording") {
    dictationRecordingStartedAt = Date.now();
  }

  if (isRecording) {
    elapsed?.classList.remove("hidden");
    updateDictationElapsed();
    if (!dictationElapsedTimer) {
      dictationElapsedTimer = window.setInterval(updateDictationElapsed, 1000);
    }
  } else {
    elapsed?.classList.add("hidden");
    if (dictationElapsedTimer) {
      window.clearInterval(dictationElapsedTimer);
      dictationElapsedTimer = undefined;
    }
    if (phase === "idle" || phase === "error") {
      dictationRecordingStartedAt = null;
      if (elapsed) elapsed.textContent = "00:00";
    }
  }

  lastDictationPhase = phase;
}

function renderDictationError(message: string) {
  const notice = $("#dictation-notice");
  const noticeText = $("#dictation-notice-text");
  if (noticeText) noticeText.textContent = message;
  notice?.classList.remove("hidden");

  const control = $("#toggle-btn") as HTMLButtonElement | null;
  const label = $("#console-title");
  const shortcut = $("#shortcut-kbd");
  if (control) {
    control.className = "dictation-control error";
    control.disabled = false;
    control.title = message;
    control.setAttribute("aria-label", `重试听写：${message}`);
  }
  if (label) label.textContent = "重试听写";
  shortcut?.classList.add("hidden");
  syncDictationElapsed("error");
}

/* ---------------- 状态渲染 ---------------- */

function applyState(state: UiState) {
  const permissionsWereComplete = currentState?.onboardingCompleted === true;
  currentState = state;

  const historyTextSize = ["compact", "standard", "large"].includes(state.historyTextSize)
    ? state.historyTextSize
    : "standard";
  document.documentElement.dataset.historyTextSize = historyTextSize;
  const historyTextSizeSelect = $("#history-text-size") as HTMLSelectElement | null;
  if (historyTextSizeSelect && document.activeElement !== historyTextSizeSelect) {
    historyTextSizeSelect.value = historyTextSize;
  }

  const kbd = $("#shortcut-kbd");
  if (kbd) kbd.textContent = formatShortcut(state.shortcut || "Alt+Space");

  const phase = state.phase || "idle";
  const controlPhase = phase;
  const consoleTitle = $("#console-title");
  if (consoleTitle) {
    if (phase === "recording") {
      consoleTitle.textContent = state.recognitionPhase === "streaming" ? "正在听写" : "正在录音";
    }
    else if (phase === "starting") consoleTitle.textContent = "正在启动录音…";
    else if (phase === "connecting") consoleTitle.textContent = "正在连接…";
    else if (phase === "finalizing") consoleTitle.textContent = "正在整理文字…";
    else if (phase === "error") consoleTitle.textContent = "重试听写";
    else consoleTitle.textContent = "开始听写";
  }

  const toggle = $("#toggle-btn") as HTMLButtonElement | null;
  if (toggle) {
    toggle.className = `dictation-control ${controlPhase}`;
    toggle.disabled = phase === "finalizing";
    toggle.title = state.status || phaseLabel(phase);
    toggle.setAttribute("aria-pressed", String(phase === "starting" || phase === "recording"));
    toggle.setAttribute(
      "aria-label",
      phase === "starting" || phase === "recording"
        ? "结束听写"
        : phase === "error"
            ? `重试听写：${state.status || "上次听写出错"}`
            : state.status || "开始听写",
    );
  }

  kbd?.classList.toggle("hidden", phase !== "idle");
  syncDictationElapsed(phase);

  const notice = $("#dictation-notice");
  const noticeText = $("#dictation-notice-text");
  if (phase === "error") {
    if (noticeText) noticeText.textContent = state.status || "请检查听写设置后重试。";
    notice?.classList.remove("hidden");
  } else {
    notice?.classList.add("hidden");
  }

  const autostart = $("#autostart-toggle") as HTMLInputElement | null;
  if (autostart && document.activeElement !== autostart) {
    autostart.checked = !!state.launchAtLogin;
  }

  const outputMuteRow = $("#output-mute-row");
  outputMuteRow?.classList.toggle("hidden", !state.systemAudioMuteSupported);
  const outputMute = $("#output-mute-toggle") as HTMLInputElement | null;
  if (outputMute && document.activeElement !== outputMute) {
    outputMute.checked = !!state.muteSystemAudioDuringDictation;
  }

  if (!recordingShortcut) {
    const scBtn = $("#shortcut-btn");
    if (scBtn) scBtn.textContent = formatShortcut(state.shortcut || "Alt+Space");
  }

  const credentialStatus = state.volcCredentialStatus ||
    (state.hasVolcApiKey ? "configured" : "missing");
  const credentialPresentation = {
    missing: {
      className: "disconnected",
      label: "未配置",
      description: "首次使用需要配置你自己的豆包语音 API Key。",
    },
    configured: {
      className: "configured",
      label: "已配置 · 待验证",
      description: "API Key 已安全读取；请验证当前仍可使用。",
    },
    verified: {
      className: "connected",
      label: "验证通过",
      description: "最近一次豆包语音服务连接验证通过。",
    },
    failed: {
      className: "failed",
      label: "验证失败",
      description: "最近一次连接失败；请重新验证或更换 API Key。",
    },
    unavailable: {
      className: "failed",
      label: "凭据不可用",
      description: "无法读取已保存的 API Key，请重新配置。",
    },
  }[credentialStatus];
  const volcMasked = $("#masked-volc-key");
  if (volcMasked) {
    volcMasked.className = `service-status ${credentialPresentation.className}`;
    volcMasked.innerHTML = `<i></i>${credentialPresentation.label}`;
  }
  const credentialDescription = state.volcCredentialWarning || credentialPresentation.description;
  const credentialHint = $("#volc-credential-hint");
  if (credentialHint) {
    credentialHint.textContent = credentialDescription;
    credentialHint.classList.toggle("warn", !!state.volcCredentialWarning);
  }
  const environmentManaged = state.volcCredentialSource === "environment";
  const editVolcButton = $("#edit-volc-btn") as HTMLButtonElement | null;
  if (editVolcButton) {
    editVolcButton.textContent = environmentManaged
      ? "由开发环境管理"
      : "更换 API Key";
    editVolcButton.disabled = environmentManaged;
    editVolcButton.classList.toggle("hidden", !state.hasVolcApiKey);
  }
  $("#test-volc-btn")?.classList.toggle("hidden", !state.hasVolcApiKey);
  $("#remove-volc-btn")?.classList.toggle(
    "hidden",
    !state.hasVolcApiKey || environmentManaged,
  );
  $("#volc-credential-editor")?.classList.toggle(
    "hidden",
    environmentManaged || (state.hasVolcApiKey && !credentialEditorOpen),
  );
  $("#cancel-volc-btn")?.classList.toggle(
    "hidden",
    !state.hasVolcApiKey || !credentialEditorOpen,
  );

  const onboardingHint = $("#ob-key-desc");
  if (onboardingHint) {
    onboardingHint.textContent = state.volcCredentialWarning || (credentialStatus === "missing"
      ? "可以跳过，稍后再到设置中配置；本地录音不受影响。"
      : credentialPresentation.description);
    onboardingHint.classList.toggle(
      "warn",
      !!state.volcCredentialWarning || ["failed", "unavailable"].includes(credentialStatus),
    );
  }
  const onboardingStatus = $("#ob-key-status");
  if (onboardingStatus) {
    onboardingStatus.className = `service-status ${credentialPresentation.className}`;
    onboardingStatus.innerHTML = `<i></i>${credentialPresentation.label}`;
  }
  const onboardingEditButton = $("#ob-edit-volc-key") as HTMLButtonElement | null;
  if (onboardingEditButton) {
    onboardingEditButton.textContent = environmentManaged ? "由开发环境管理" : "更换 API Key";
    onboardingEditButton.disabled = environmentManaged;
  }
  if (environmentManaged) onboardingCredentialEditorOpen = false;
  $("#ob-key-summary")?.classList.toggle(
    "hidden",
    !state.hasVolcApiKey || onboardingCredentialEditorOpen,
  );
  $("#ob-key-editor")?.classList.toggle(
    "hidden",
    state.hasVolcApiKey && !onboardingCredentialEditorOpen,
  );
  $("#ob-cancel-volc-key")?.classList.toggle(
    "hidden",
    !state.hasVolcApiKey || !onboardingCredentialEditorOpen,
  );

  renderOnboardingCompletion(state);
  updateOnboardingNextLabel();
  const volcResource = $("#volc-resource-id") as HTMLInputElement | null;
  if (volcResource && document.activeElement !== volcResource) {
    volcResource.value = state.volcResourceId || "volc.seedasr.sauc.duration";
  }
  const volcTable = $("#volc-table-id") as HTMLInputElement | null;
  if (volcTable && document.activeElement !== volcTable) {
    volcTable.value = state.volcBoostingTableId || "";
  }
  const syncBtn = $("#hotwords-sync-volc") as HTMLButtonElement | null;
  if (syncBtn) {
    const canSync =
      !!state.hasVolcApiKey && !!(state.volcBoostingTableId || "").trim();
    syncBtn.classList.toggle("hidden", !canSync);
    syncBtn.disabled = !canSync;
    syncBtn.title = canSync
      ? "手动同步热词表到云端（替换词仅本地）"
      : "需要先连接豆包语音并配置云端热词表";
  }

  const punctuation = $("#semantic-punctuation") as HTMLInputElement | null;
  if (punctuation && document.activeElement !== punctuation) {
    punctuation.checked = !!state.semanticPunctuationEnabled;
  }
  const smoothing = $("#semantic-smoothing") as HTMLInputElement | null;
  if (smoothing && document.activeElement !== smoothing) {
    smoothing.checked = !!state.semanticSmoothingEnabled;
  }
  const silence = $("#silence-ms") as HTMLSelectElement | null;
  if (silence && document.activeElement !== silence) {
    silence.querySelector("option[data-current-value]")?.remove();
    const value = String(state.maxSentenceSilenceMs ?? 0);
    if (!Array.from(silence.options).some((option) => option.value === value)) {
      const seconds = (Number(value) / 1000).toLocaleString("zh-CN", {
        maximumFractionDigits: 1,
      });
      const currentOption = new Option(`当前 · ${seconds} 秒`, value);
      currentOption.dataset.currentValue = "true";
      silence.add(currentOption, 0);
    }
    silence.value = value;
  }

  const gain = $("#gain-db") as HTMLInputElement | null;
  if (gain && document.activeElement !== gain) {
    gain.value = String(state.inputGainDb || 0);
  }
  const gainLabel = $("#gain-db-label");
  if (gainLabel) {
    gainLabel.textContent = state.inputGainDb > 0 ? `增强 +${state.inputGainDb} dB` : "默认";
  }

  const ob = $("#onboarding");
  if (ob) ob.classList.toggle("hidden", !!state.onboardingCompleted);
  if (permissionsWereComplete && !state.onboardingCompleted) {
    onboardingMicVerified = false;
    onboardingMicNeedsManualRecovery = false;
    onboardingAccessibilityGranted = false;
    goObStep(state.status.includes("辅助功能") && !state.status.includes("麦克风") ? 2 : 1);
  }

  applyMicUi(state);
}

function applyMicUi(state: UiState) {
  const select = $("#mic-select") as HTMLSelectElement | null;
  if (select && document.activeElement !== select) {
    const devices = state.inputDevices || [];
    select.innerHTML = "";
    if (devices.length === 0) {
      const opt = document.createElement("option");
      opt.value = "";
      opt.textContent = "未检测到麦克风";
      select.appendChild(opt);
      select.disabled = true;
    } else {
      select.disabled = false;
      for (const device of devices) {
        const opt = document.createElement("option");
        opt.value = device.id;
        opt.textContent = device.isDefault ? `${device.name}（默认）` : device.name;
        select.appendChild(opt);
      }
      const exists = devices.some((d) => d.id === state.selectedInputDeviceId);
      const fallback = devices.find((d) => d.isDefault) || devices[0];
      select.value = exists ? state.selectedInputDeviceId : fallback.id;
    }
  }

  const meter = $("#mic-meter");
  if (meter) meter.classList.toggle("hidden", !state.micTesting);

  const testBtn = $("#mic-test-btn") as HTMLButtonElement | null;
  if (testBtn) {
    testBtn.textContent = state.micTesting ? "测试中" : "测试";
    testBtn.disabled = state.micTesting;
  }
  const stopBtn = $("#mic-test-stop-btn");
  if (stopBtn) stopBtn.classList.toggle("hidden", !state.micTesting);

  if (!state.micTesting) renderLevel(0);

  // Onboarding 麦克风控件与主设置保持一致
  const obSelect = $("#ob-mic-select") as HTMLSelectElement | null;
  if (obSelect && document.activeElement !== obSelect) {
    const devices = state.inputDevices || [];
    obSelect.innerHTML = "";
    if (devices.length === 0) {
      const opt = document.createElement("option");
      opt.value = "";
      opt.textContent = state.onboardingCompleted
        ? "未检测到麦克风"
        : "授权并开始测试后可选择麦克风";
      obSelect.appendChild(opt);
      obSelect.disabled = true;
    } else {
      obSelect.disabled = false;
      for (const device of devices) {
        const opt = document.createElement("option");
        opt.value = device.id;
        opt.textContent = device.isDefault ? `${device.name}（默认）` : device.name;
        obSelect.appendChild(opt);
      }
      const exists = devices.some((d) => d.id === state.selectedInputDeviceId);
      const fallback = devices.find((d) => d.isDefault) || devices[0];
      obSelect.value = exists ? state.selectedInputDeviceId : fallback.id;
    }
  }

  const obMeter = $("#ob-mic-meter");
  if (obMeter) obMeter.classList.toggle("hidden", !state.micTesting);

  const obTestBtn = $("#ob-mic-test-btn") as HTMLButtonElement | null;
  if (obTestBtn) {
    obTestBtn.textContent = state.micTesting ? "测试中…" : "测试麦克风";
    obTestBtn.disabled = state.micTesting;
    obTestBtn.classList.toggle("hidden", onboardingMicNeedsManualRecovery);
  }
  const obStopBtn = $("#ob-mic-stop-btn");
  if (obStopBtn) obStopBtn.classList.toggle("hidden", !state.micTesting);
  $("#ob-mic-recovery")?.classList.toggle(
    "hidden",
    !onboardingMicNeedsManualRecovery || state.micTesting,
  );

  if (!state.micTesting) renderLevel(0, $("#ob-level-segments"));
}

/* ---------------- 电平段条 ---------------- */

function ensureSegments() {
  const host = $("#level-segments");
  if (!host || host.childElementCount > 0) return;
  for (let i = 0; i < 12; i++) {
    const seg = document.createElement("div");
    seg.className = "seg";
    host.appendChild(seg);
  }
}

function ensureSegmentsIn(host: Element | null | undefined) {
  if (!host || host.childElementCount > 0) return;
  for (let i = 0; i < 12; i++) {
    const seg = document.createElement("div");
    seg.className = "seg";
    host.appendChild(seg);
  }
}

function renderLevel(level: number, host?: Element | null) {
  const target = host ?? $("#level-segments");
  ensureSegmentsIn(target);
  if (!target) return;
  const active = Math.round(Math.max(0, Math.min(1, level)) * 12);
  Array.from(target.children).forEach((child, index) => {
    child.classList.toggle("on", index < active);
  });
}

/* ---------------- 首页：统计 + 历史 ---------------- */

async function refreshHistory() {
  try {
    const data = await invoke<HistoryData>("get_history");
    renderStats(data.stats);
    renderHistory(data.records);
  } catch (error) {
    console.error("load history failed", error);
  }
}

function renderStats(stats: HistoryStats) {
  const sessions = $("#stat-sessions");
  if (sessions) sessions.textContent = `${stats.sessions} 次`;
  const duration = $("#stat-duration");
  if (duration) duration.textContent = formatDuration(stats.totalDurationMs);
  const chars = $("#stat-chars");
  if (chars) chars.textContent = formatChars(stats.totalChars);
}

function renderHistory(records: HistoryRecord[]) {
  const host = $("#history");
  if (!host) return;
  historyRecords = records;
  if (selectedHistoryId && !records.some((record) => record.id === selectedHistoryId)) {
    selectedHistoryId = null;
    const detail = $("#history-detail") as HTMLDialogElement | null;
    if (detail?.open) detail.close();
  }
  if (historyPlayback && !records.some((record) => record.id === historyPlayback?.recordId)) {
    stopHistoryPlayback();
  }
  host.innerHTML = "";

  const query = historyQuery.trim().toLocaleLowerCase();
  const visibleRecords = query
    ? records.filter((record) => record.text.toLocaleLowerCase().includes(query))
    : records;
  const count = $("#history-count");
  if (count) {
    count.textContent = query ? `${visibleRecords.length} / ${records.length} 条` : `${records.length} 条`;
  }
  if (visibleRecords.length === 0) {
    const empty = document.createElement("div");
    empty.className = "history-empty";
    const title = document.createElement("strong");
    title.textContent = query ? "没有找到相关记录" : "还没有听写记录";
    const hint = document.createElement("span");
    hint.textContent = query
      ? "换一个关键词再试试。"
      : "按下快捷键，说出你的第一段文字。";
    empty.append(title, hint);
    host.appendChild(empty);
    return;
  }

  const groups: { key: string; label: string; items: HistoryRecord[] }[] = [];
  for (const record of visibleRecords) {
    const d = new Date(record.finishedAtMs);
    const key = `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
    let group = groups.find((g) => g.key === key);
    if (!group) {
      group = { key, label: dayLabel(d), items: [] };
      groups.push(group);
    }
    group.items.push(record);
  }

  for (const group of groups) {
    const day = document.createElement("div");
    const label = document.createElement("div");
    label.className = "history-group-label";
    label.textContent = group.label;
    day.appendChild(label);

    for (const record of group.items) {
      const hasPlayableAudio = !!record.audio && !record.audioMissing;
      const row = document.createElement("div");
      row.className = `history-row${record.id === selectedHistoryId ? " active" : ""}`;
      row.dataset.historyRow = record.id;
      row.tabIndex = 0;
      row.setAttribute("role", "button");
      row.setAttribute("aria-label", `查看 ${timeLabel(new Date(record.finishedAtMs))} 的听写记录`);
      const openDetail = () => openHistoryDetail(record);
      row.addEventListener("click", openDetail);
      row.addEventListener("keydown", (event) => {
        if (event.key !== "Enter" && event.key !== " ") return;
        event.preventDefault();
        openDetail();
      });

      const meta = document.createElement("div");
      meta.className = "history-row-meta";
      const time = document.createElement("span");
      time.className = "history-row-time";
      time.textContent = timeLabel(new Date(record.finishedAtMs));
      const duration = document.createElement("span");
      duration.className = `history-row-duration${hasPlayableAudio ? " audio" : ""}`;
      duration.textContent = formatClipDuration(record.durationMs);
      meta.append(time, duration);

      const text = document.createElement("div");
      text.className = "history-row-text";
      text.textContent = record.text
        || (record.recordingError
          ? "本地录音异常 · 文件可能不完整"
          : record.recognitionError
            ? "录音已保存 · 实时识别未完成"
            : "本次录音未生成文字");

      const actions = document.createElement("div");
      actions.className = "history-row-actions";
      if (record.text.trim()) {
        const copy = document.createElement("button");
        copy.type = "button";
        copy.className = "history-row-action";
        copy.textContent = "复制";
        copy.addEventListener("click", (event) => {
          event.stopPropagation();
          void copyHistoryText(record.text, copy);
        });
        actions.appendChild(copy);
      }

      if (hasPlayableAudio) {
        const play = document.createElement("button");
        play.type = "button";
        play.className = "history-row-action";
        play.dataset.historyPlay = record.id;
        play.textContent = "播放";
        play.addEventListener("click", (event) => {
          event.stopPropagation();
          void toggleHistoryPlayback(record.id);
        });
        actions.appendChild(play);
      }

      row.append(meta, text, actions);
      day.appendChild(row);
    }
    host.appendChild(day);
  }
  syncPlaybackUi();
}

function openHistoryDetail(record: HistoryRecord) {
  selectedHistoryId = record.id;
  document.querySelectorAll<HTMLElement>("[data-history-row]").forEach((row) => {
    row.classList.toggle("active", row.dataset.historyRow === record.id);
  });
  renderHistoryDetail(record);
}

function renderHistoryDetail(record: HistoryRecord) {
  const host = $("#history-detail") as HTMLDialogElement | null;
  if (!host) return;
  host.innerHTML = "";

  const date = new Date(record.finishedAtMs);
  const head = document.createElement("header");
  head.className = "detail-head";
  const heading = document.createElement("div");
  const title = document.createElement("h2");
  title.textContent = `${dayLabel(date)} ${timeLabel(date)}`;
  const meta = document.createElement("div");
  meta.className = "detail-meta";
  meta.textContent = [
    formatClipDuration(record.durationMs),
    `${record.charCount} 字`,
  ].filter(Boolean).join(" · ");
  heading.append(title, meta);

  const actions = document.createElement("div");
  actions.className = "detail-actions";
  if (record.text.trim()) {
    const copy = document.createElement("button");
    copy.type = "button";
    copy.className = "detail-action";
    copy.textContent = "复制文字";
    copy.addEventListener("click", () => void copyHistoryText(record.text, copy));
    actions.appendChild(copy);
  }
  const close = document.createElement("button");
  close.type = "button";
  close.className = "detail-close";
  close.setAttribute("aria-label", "关闭详情");
  close.textContent = "×";
  close.addEventListener("click", () => host.close());
  actions.appendChild(close);
  head.append(heading, actions);

  const content = document.createElement("div");
  content.className = "detail-content";

  if (record.audio && !record.audioMissing) {
    const player = document.createElement("div");
    player.className = "detail-player";
    const playerMain = document.createElement("div");
    playerMain.className = "detail-player-main";
    const play = document.createElement("button");
    play.type = "button";
    play.className = "detail-play";
    play.dataset.historyPlay = record.id;
    play.setAttribute("aria-label", "播放录音");
    play.textContent = "▶";
    play.addEventListener("click", () => void toggleHistoryPlayback(record.id));

    const timeline = document.createElement("div");
    timeline.className = "detail-timeline";
    const progress = document.createElement("input");
    progress.type = "range";
    progress.className = "detail-progress";
    progress.id = "detail-progress";
    progress.dataset.recordId = record.id;
    progress.min = "0";
    progress.max = String(Math.max(0.1, record.audio.durationMs / 1000));
    progress.step = "0.1";
    progress.value = "0";
    progress.setAttribute("aria-label", "录音播放进度");
    const times = document.createElement("div");
    times.className = "detail-times";
    const elapsed = document.createElement("span");
    elapsed.id = "detail-elapsed";
    elapsed.textContent = "0:00";
    const total = document.createElement("span");
    total.id = "detail-total";
    total.textContent = formatClipDuration(record.audio.durationMs);
    times.append(elapsed, total);
    timeline.append(progress, times);
    progress.addEventListener("input", () => {
      elapsed.textContent = formatPlaybackTime(Number(progress.value));
      if (historyPlayback?.recordId === record.id) {
        historyPlayback.audio.currentTime = Number(progress.value);
      }
    });
    progress.addEventListener("change", () => {
      void seekHistoryPlayback(record.id, Number(progress.value));
    });

    const speed = document.createElement("button");
    speed.type = "button";
    speed.className = "detail-speed";
    speed.id = "detail-speed";
    speed.textContent = `${historyPlaybackRate}×`;
    speed.setAttribute("aria-label", "切换播放速度");
    speed.addEventListener("click", cycleHistoryPlaybackRate);
    playerMain.append(play, timeline, speed);
    player.appendChild(playerMain);
    content.appendChild(player);
  } else {
    const noAudio = document.createElement("div");
    noAudio.className = "detail-no-audio";
    noAudio.textContent = record.audioMissing
      ? "录音文件已在本地录音文件夹中被删除或移动"
      : "这是一条旧版文字记录，没有关联的本地录音";
    content.appendChild(noAudio);
  }

  if (record.recordingError) {
    const recordingNotice = document.createElement("div");
    recordingNotice.className = "detail-no-audio";
    recordingNotice.textContent = `本地录音异常：${record.recordingError}`;
    content.appendChild(recordingNotice);
  }

  if (record.recognitionError) {
    const recognitionNotice = document.createElement("div");
    recognitionNotice.className = "detail-no-audio";
    recognitionNotice.textContent = `实时识别未完成：${record.recognitionError}`;
    content.appendChild(recognitionNotice);
  }

  const transcriptLabel = document.createElement("div");
  transcriptLabel.className = "detail-section-label";
  transcriptLabel.textContent = "听写文字";
  const transcript = document.createElement("div");
  transcript.className = "detail-transcript";
  transcript.textContent = record.text || "本次录音未生成听写文字。";
  content.append(transcriptLabel, transcript);

  const info = document.createElement("details");
  info.className = "detail-info";
  const infoSummary = document.createElement("summary");
  infoSummary.textContent = "记录信息";
  const infoGrid = document.createElement("dl");
  infoGrid.className = "detail-info-grid";
  const addInfo = (label: string, value: string) => {
    const term = document.createElement("dt");
    term.textContent = label;
    const description = document.createElement("dd");
    description.textContent = value;
    infoGrid.append(term, description);
  };
  addInfo("完成时间", date.toLocaleString("zh-CN"));
  addInfo("文字", `${record.charCount} 字`);
  if (record.audio && !record.audioMissing) {
    addInfo(
      "本地录音",
      `WAV · ${record.audio.sampleRateHz / 1000} kHz · ${formatFileSize(record.audio.sizeBytes)}`,
    );
  } else if (record.audioMissing) {
    addInfo("本地录音", "文件已被删除或移动");
  } else {
    addInfo("本地录音", "旧版记录未关联录音");
  }
  info.append(infoSummary, infoGrid);
  content.appendChild(info);

  const footer = document.createElement("div");
  footer.className = "detail-footer";
  const footerStart = document.createElement("div");
  if (record.audio && !record.audioMissing) {
    const reveal = document.createElement("button");
    reveal.type = "button";
    reveal.className = "detail-action";
    reveal.textContent = "显示录音文件";
    reveal.addEventListener("click", () => {
      reveal.disabled = true;
      void invoke("reveal_history_audio", { recordId: record.id })
        .catch((error) => showToast(String(error), "error"))
        .finally(() => (reveal.disabled = false));
    });
    footerStart.appendChild(reveal);
  }
  const remove = document.createElement("button");
  remove.type = "button";
  remove.className = "detail-action danger";
  remove.textContent = "删除这条记录";
  remove.addEventListener("click", () => void deleteHistoryRecord(record.id));
  footer.append(footerStart, remove);
  content.appendChild(footer);

  host.append(head, content);
  if (!host.open) host.showModal();
  syncPlaybackUi();
}

async function deleteHistoryRecord(recordId: string) {
  if (!window.confirm("删除这条应用内听写记录？本地录音文件会继续保留。")) return;
  if (historyPlayback?.recordId === recordId) stopHistoryPlayback();
  try {
    const data = await invoke<HistoryData>("delete_history_record", { recordId });
    const detail = $("#history-detail") as HTMLDialogElement | null;
    if (detail?.open) detail.close();
    selectedHistoryId = null;
    renderStats(data.stats);
    renderHistory(data.records);
    showToast("听写记录已删除", "success");
  } catch (error) {
    await refreshHistory();
    showToast(`删除失败：${String(error)}`, "error");
  }
}

function stopHistoryPlayback() {
  historyPlaybackLoadGeneration += 1;
  historyPlaybackLoadingId = null;
  if (historyPlayback) {
    historyPlayback.audio.pause();
    historyPlayback.audio.currentTime = 0;
    URL.revokeObjectURL(historyPlayback.objectUrl);
  }
  historyPlayback = null;
  syncPlaybackUi();
}

function syncPlaybackUi() {
  const activeId = historyPlayback?.recordId ?? null;
  const playing = !!historyPlayback && !historyPlayback.audio.paused;

  document.querySelectorAll<HTMLButtonElement>("[data-history-play]").forEach((button) => {
    const recordId = button.dataset.historyPlay ?? "";
    const current = recordId === activeId;
    const loading = recordId === historyPlaybackLoadingId;
    const detailButton = button.classList.contains("detail-play");
    button.disabled = loading;
    button.textContent = loading ? (detailButton ? "…" : "加载中") : current && playing
      ? (detailButton ? "Ⅱ" : "暂停")
      : (detailButton ? "▶" : "播放");
    button.setAttribute("aria-label", current && playing ? "暂停录音" : "播放录音");
  });

  document.querySelectorAll<HTMLElement>("[data-history-row]").forEach((row) => {
    row.classList.toggle("playing", row.dataset.historyRow === activeId && playing);
  });

  const progress = $("#detail-progress") as HTMLInputElement | null;
  if (progress) {
    const recordId = progress.dataset.recordId ?? "";
    const record = historyRecords.find((item) => item.id === recordId);
    const current = recordId === activeId;
    const fallbackDuration = (record?.audio?.durationMs ?? record?.durationMs ?? 0) / 1000;
    const audioDuration = current && Number.isFinite(historyPlayback?.audio.duration)
      ? historyPlayback?.audio.duration ?? fallbackDuration
      : fallbackDuration;
    progress.max = String(Math.max(0.1, audioDuration));
    progress.value = String(current ? historyPlayback?.audio.currentTime ?? 0 : 0);
    const elapsed = $("#detail-elapsed");
    if (elapsed) elapsed.textContent = formatPlaybackTime(Number(progress.value));
    const total = $("#detail-total");
    if (total) total.textContent = formatPlaybackTime(audioDuration);
  }

  const speed = $("#detail-speed");
  if (speed) speed.textContent = `${historyPlaybackRate}×`;
}

async function loadHistoryPlayback(recordId: string): Promise<HTMLAudioElement | null> {
  if (historyPlayback?.recordId === recordId) return historyPlayback.audio;
  stopHistoryPlayback();
  const loadGeneration = historyPlaybackLoadGeneration;
  historyPlaybackLoadingId = recordId;
  syncPlaybackUi();
  try {
    const wav = await invoke<ArrayBuffer>("get_history_audio", { recordId });
    if (loadGeneration !== historyPlaybackLoadGeneration) return null;
    const objectUrl = URL.createObjectURL(new Blob([wav], { type: "audio/wav" }));
    const audio = new Audio(objectUrl);
    audio.preload = "metadata";
    audio.playbackRate = historyPlaybackRate;
    historyPlayback = { recordId, audio, objectUrl };
    audio.addEventListener("play", syncPlaybackUi);
    audio.addEventListener("pause", syncPlaybackUi);
    audio.addEventListener("timeupdate", syncPlaybackUi);
    audio.addEventListener("loadedmetadata", syncPlaybackUi);
    audio.addEventListener("ratechange", syncPlaybackUi);
    audio.addEventListener("ended", stopHistoryPlayback, { once: true });
    audio.addEventListener(
      "error",
      () => {
        console.error("play history audio failed", audio.error);
        showToast("录音播放失败", "error");
        stopHistoryPlayback();
      },
      { once: true },
    );
    return audio;
  } catch (error) {
    if (loadGeneration !== historyPlaybackLoadGeneration) return null;
    console.error("load history audio failed", error);
    showToast(`录音加载失败：${String(error)}`, "error");
    return null;
  } finally {
    if (loadGeneration === historyPlaybackLoadGeneration) {
      historyPlaybackLoadingId = null;
      syncPlaybackUi();
    }
  }
}

async function toggleHistoryPlayback(recordId: string) {
  if (historyPlayback?.recordId === recordId) {
    if (historyPlayback.audio.paused) {
      try {
        await historyPlayback.audio.play();
      } catch (error) {
        console.error("resume history audio failed", error);
        showToast("录音播放失败", "error");
        stopHistoryPlayback();
      }
    } else {
      historyPlayback.audio.pause();
    }
    syncPlaybackUi();
    return;
  }

  const audio = await loadHistoryPlayback(recordId);
  if (!audio) return;
  try {
    await audio.play();
  } catch (error) {
    console.error("play history audio failed", error);
    showToast("录音播放失败", "error");
    stopHistoryPlayback();
  }
  syncPlaybackUi();
}

async function seekHistoryPlayback(recordId: string, seconds: number) {
  const audio = await loadHistoryPlayback(recordId);
  if (!audio) return;
  audio.currentTime = Math.max(0, Math.min(seconds, Number.isFinite(audio.duration) ? audio.duration : seconds));
  syncPlaybackUi();
}

function cycleHistoryPlaybackRate() {
  const rates = [1, 1.5, 2];
  const index = rates.indexOf(historyPlaybackRate);
  historyPlaybackRate = rates[(index + 1) % rates.length];
  if (historyPlayback) historyPlayback.audio.playbackRate = historyPlaybackRate;
  syncPlaybackUi();
}

/* ---------------- 词库 ---------------- */

async function refreshHotwords() {
  try {
    hotwords = await invoke<string[]>("get_hotwords");
    renderHotwords();
  } catch (error) {
    console.error("load hotwords failed", error);
  }
}

function renderHotwords() {
  const grid = $("#hotwords-grid");
  const count = $("#hotwords-count");
  if (count) count.textContent = `${hotwords.length}/${hotwordLimit()} 热词`;
  const textarea = $("#hotwords-input") as HTMLTextAreaElement | null;
  if (textarea) {
    textarea.placeholder =
      "每行一个热词（识别用）。\n会自动去空格/标点，如 B roll → Broll。\n格式还原请到右侧「替换词」手动配置。";
  }
  if (!grid) return;
  grid.innerHTML = "";

  const query = hotwordFilter.trim().toLowerCase();
  const list = query ? hotwords.filter((w) => w.toLowerCase().includes(query)) : hotwords;

  if (list.length === 0) {
    const empty = document.createElement("div");
    empty.className = "empty-hint";
    empty.style.gridColumn = "1 / -1";
    empty.textContent = query ? "没有匹配的热词" : "还没有热词，在上方输入框添加吧。";
    grid.appendChild(empty);
    return;
  }

  for (const word of list) {
    const card = document.createElement("div");
    card.className = "word-card";
    const label = document.createElement("span");
    label.textContent = word;
    const del = document.createElement("button");
    del.className = "word-del";
    del.title = "删除";
    del.textContent = "×";
    del.addEventListener("click", () => void removeHotword(word));
    card.append(label, del);
    grid.appendChild(card);
  }
}

function normalizeHotwordForEngine(word: string): string {
  const trimmed = word.trim();
  // 火山热词：自动归一为识别形。
  return Array.from(trimmed)
    .filter((c) => /[\p{L}\p{N}]/u.test(c))
    .join("");
}

function parseHotwordInput(): string[] {
  const textarea = $("#hotwords-input") as HTMLTextAreaElement | null;
  if (!textarea) return [];
  return textarea.value
    .split("\n")
    .map((line) => normalizeHotwordForEngine(line))
    .filter(isValidHotword);
}

function syncEditorButtons() {
  const textarea = $("#hotwords-input") as HTMLTextAreaElement | null;
  const has = !!textarea && textarea.value.trim().length > 0;
  const save = $("#hotwords-save") as HTMLButtonElement | null;
  const clear = $("#hotwords-clear") as HTMLButtonElement | null;
  if (save) save.disabled = !has;
  if (clear) clear.disabled = !has;
}


async function persistHotwords(
  words: string[],
  opts?: { quiet?: boolean },
): Promise<SaveHotwordsResult> {
  const result = await invoke<SaveHotwordsResult>("save_hotwords", { words });
  hotwords = result.words;
  renderHotwords();
  if (!opts?.quiet) {
    try {
      const state = await invoke<UiState>("get_state");
      applyState(state);
    } catch {
      // ignore
    }
  }
  if (result.syncError) {
    console.warn("auto sync volc hotwords failed:", result.syncError);
  }
  return result;
}

async function saveHotwordsFromEditor() {
  const incoming = parseHotwordInput();
  if (incoming.length === 0) return;
  const merged = [...hotwords];
  for (const word of incoming) {
    if (!merged.includes(word)) merged.push(word);
  }
  try {
    await persistHotwords(merged.slice(0, hotwordLimit()));
    const textarea = $("#hotwords-input") as HTMLTextAreaElement | null;
    if (textarea) textarea.value = "";
    syncEditorButtons();
  } catch (error) {
    console.error("save hotwords failed", error);
    window.alert(`保存词库失败：${error}`);
  }
}


async function syncVolcHotwordTable() {
  const btn = $("#hotwords-sync-volc") as HTMLButtonElement | null;
  if (btn) btn.disabled = true;
  try {
    // 手动同步：先规范化保存（会自动同步一次），再强制再同步一次确保最新。
    const result = await invoke<SaveHotwordsResult>("save_hotwords", { words: hotwords });
    hotwords = result.words;
    renderHotwords();
    if (result.synced) {
      const state = await invoke<UiState>("get_state");
      applyState(state);
      window.alert(result.message || "已同步到云端。");
      return;
    }
    const state = await invoke<UiState>("sync_volc_hotword_table");
    applyState(state);
    window.alert(state.status || "已同步到云端。");
  } catch (error) {
    console.error("sync volc hotword table failed", error);
    window.alert(`同步失败：${error}`);
  } finally {
    if (btn) btn.disabled = false;
  }
}

/* ---------------- 词库文件批量导入 ---------------- */

/** 读取文件文本，自动识别 UTF-8 / GBK 编码。 */
async function readHotwordFile(file: File): Promise<string> {
  const buffer = await file.arrayBuffer();
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(buffer);
  } catch {
    // 中文环境常见的 GBK 编码降级（仅在 UTF-8 解码失败时使用）。
    return new TextDecoder("gbk").decode(buffer);
  }
}

/**
 * 按行解析词库文件：一行一个词。
 * 自动去掉 BOM、统一换行符、忽略空行，并把不符合官方规范的行单独挑出来。
 */
function parseHotwordFile(
  text: string,
): { valid: string[]; invalid: string[] } {
  const valid: string[] = [];
  const invalid: string[] = [];
  const normalized = text.replace(/\uFEFF/g, "").replace(/\r\n?/g, "\n");
  for (const raw of normalized.split("\n")) {
    const word = normalizeHotwordForEngine(raw);
    if (!word) continue; // 空行 / 纯空白行直接忽略
    if (isValidHotword(word)) valid.push(word);
    else invalid.push(word);
  }
  return { valid, invalid };
}

/** 从 txt 文件批量导入词库，与现有词去重合并后保存。 */
async function importHotwordsFromFile(file: File) {
  const text = await readHotwordFile(file);
  const { valid, invalid } = parseHotwordFile(text);
  if (valid.length === 0) {
    window.alert("文件中没有找到有效词条。请使用 txt 文件，每行一个词。");
    return;
  }

  const originalCount = hotwords.length;
  const merged = [...hotwords];
  let duplicate = 0;
  for (const word of valid) {
    if (!merged.includes(word)) merged.push(word);
    else duplicate += 1;
  }
  const limit = hotwordLimit();
  const overflow = Math.max(0, merged.length - limit);
  const finalWords = merged.slice(0, limit);

  const saveResult = await persistHotwords(finalWords);

  const added = Math.max(0, hotwords.length - originalCount);
  const messages: string[] = [`导入完成：新增 ${added} 条，共 ${hotwords.length}/${limit} 条。`];
  if (saveResult.message) {
    messages.push(`- ${saveResult.message}`);
  }
  if (invalid.length > 0) {
    const sample = invalid.slice(0, 10).join("、");
    const more = invalid.length > 10 ? ` 等 ${invalid.length} 条` : "";
    messages.push(`- ${invalid.length} 条不符合规范：${sample}${more}`);
  }
  if (duplicate > 0) {
    messages.push(`- ${duplicate} 条与已有词重复，已跳过。`);
  }
  if (overflow > 0) {
    messages.push(`- 超出热词上限（${limit} 条），最后 ${overflow} 条未导入。`);
  }
  window.alert(messages.join("\n"));
}

async function removeHotword(word: string) {
  try {
    await persistHotwords(hotwords.filter((w) => w !== word));
  } catch (error) {
    console.error("remove hotword failed", error);
  }
}


/* ---------------- 替换词（用户手动配置） ---------------- */

async function refreshReplacements() {
  try {
    replacements = await invoke<ReplacementRule[]>("get_replacements");
    renderReplacements();
  } catch (error) {
    console.error("load replacements failed", error);
  }
}

function renderReplacements() {
  const grid = $("#replacements-grid");
  const count = $("#replacements-count");
  if (count) count.textContent = `${replacements.length} 条替换`;
  if (!grid) return;
  grid.innerHTML = "";
  const query = replacementFilter.trim().toLowerCase();
  const list = query
    ? replacements.filter(
        (r) =>
          r.from.toLowerCase().includes(query) ||
          r.to.toLowerCase().includes(query),
      )
    : replacements;
  if (list.length === 0) {
    const empty = document.createElement("div");
    empty.className = "empty-hint";
    empty.style.gridColumn = "1 / -1";
    empty.textContent = query
      ? "没有匹配的替换词"
      : "还没有替换词。右侧输入：识别结果|想要的写法";
    grid.appendChild(empty);
    return;
  }
  for (const rule of list) {
    const card = document.createElement("div");
    card.className = "word-card replacement-card";
    const body = document.createElement("div");
    const from = document.createElement("div");
    from.className = "rep-from";
    from.textContent = rule.from;
    const to = document.createElement("div");
    to.className = "rep-to";
    to.textContent = rule.to;
    body.append(from, to);
    const del = document.createElement("button");
    del.className = "word-del";
    del.title = "删除";
    del.textContent = "×";
    del.addEventListener("click", () => void removeReplacement(rule.from));
    card.append(body, del);
    grid.appendChild(card);
  }
}

function parseReplacementInput(): ReplacementRule[] {
  const textarea = $("#replacements-input") as HTMLTextAreaElement | null;
  if (!textarea) return [];
  const out: ReplacementRule[] = [];
  for (const raw of textarea.value.split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    const sep = line.includes("|") ? "|" : line.includes("→") ? "→" : line.includes("->") ? "->" : "";
    if (!sep) continue;
    const idx = line.indexOf(sep);
    const from = line.slice(0, idx).trim();
    const to = line.slice(idx + sep.length).trim();
    // 允许 from === to：用作长短语锁，避免短词替换误伤。
    if (!from || !to) continue;
    out.push({ from, to });
  }
  return out;
}

function syncReplacementEditorButtons() {
  const textarea = $("#replacements-input") as HTMLTextAreaElement | null;
  const has = !!textarea && textarea.value.trim().length > 0;
  const save = $("#replacements-save") as HTMLButtonElement | null;
  const clear = $("#replacements-clear") as HTMLButtonElement | null;
  if (save) save.disabled = !has;
  if (clear) clear.disabled = !has;
}

async function persistReplacements(rules: ReplacementRule[]) {
  const result = await invoke<SaveHotwordsResult>("save_replacements", { rules });
  // 重新读取规范化后的规则
  await refreshReplacements();
  try {
    const state = await invoke<UiState>("get_state");
    applyState(state);
  } catch {
    // ignore
  }
  if (result.syncError) {
    console.warn("auto sync replacements failed:", result.syncError);
  }
  return result;
}

async function saveReplacementsFromEditor() {
  const incoming = parseReplacementInput();
  if (incoming.length === 0) {
    window.alert("请按「识别结果|想要的写法」格式输入，例如：Broll|B roll");
    return;
  }
  const merged = [...replacements];
  for (const rule of incoming) {
    const idx = merged.findIndex((r) => r.from === rule.from);
    if (idx >= 0) merged[idx] = rule;
    else merged.push(rule);
  }
  try {
    const result = await persistReplacements(merged);
    const textarea = $("#replacements-input") as HTMLTextAreaElement | null;
    if (textarea) textarea.value = "";
    syncReplacementEditorButtons();
    if (result.message) {
      // 状态栏已更新；不弹窗打扰
    }
  } catch (error) {
    console.error("save replacements failed", error);
    window.alert(`保存替换词失败：${error}`);
  }
}

async function removeReplacement(from: string) {
  try {
    await persistReplacements(replacements.filter((r) => r.from !== from));
  } catch (error) {
    console.error("remove replacement failed", error);
  }
}

/* ---------------- 设置弹窗 ---------------- */

function openSettings() {
  credentialEditorOpen = !currentState?.hasVolcApiKey;
  if (currentState) applyState(currentState);
  $("#settings-modal")?.classList.remove("hidden");
  void refreshMicDevices();
}

function closeSettings() {
  if (recordingShortcut) stopRecording();
  credentialEditorOpen = false;
  const input = $("#volc-api-key") as HTMLInputElement | null;
  if (input) input.value = "";
  $("#settings-modal")?.classList.add("hidden");
}

function showShortcutError(message: string) {
  const el = $("#shortcut-error");
  if (el) {
    el.textContent = message;
    el.classList.remove("hidden");
  }
}

function hideShortcutError() {
  $("#shortcut-error")?.classList.add("hidden");
}

function startRecording() {
  recordingShortcut = true;
  hideShortcutError();
  void invoke("start_shortcut_recording").catch((error) =>
    console.error("start native shortcut recording failed", error),
  );
  const btn = $("#shortcut-btn");
  if (btn) {
    btn.classList.add("recording");
    btn.textContent = "按下快捷键（Esc 取消）";
  }
}

function stopRecording() {
  recordingShortcut = false;
  void invoke("cancel_shortcut_recording").catch((error) =>
    console.error("cancel native shortcut recording failed", error),
  );
  const btn = $("#shortcut-btn");
  if (btn) {
    btn.classList.remove("recording");
    btn.textContent = formatShortcut(currentState?.shortcut || "Alt+Space");
  }
}

function acceleratorFromEvent(e: KeyboardEvent): string | null {
  const mods: string[] = [];
  if (e.ctrlKey) mods.push("Control");
  if (e.altKey) mods.push("Alt");
  if (e.shiftKey) mods.push("Shift");
  if (e.metaKey) mods.push("Meta");

  const code = e.code;
  let key: string | null = null;
  if (code === "Space") key = "Space";
  else if (/^Key[A-Z]$/.test(code)) key = code.slice(3);
  else if (/^Digit[0-9]$/.test(code)) key = code.slice(5);
  else if (/^F([1-9]|1[0-2])$/.test(code)) key = code;
  else if (code === "ArrowLeft") key = "Left";
  else if (code === "ArrowRight") key = "Right";
  else if (code === "ArrowUp") key = "Up";
  else if (code === "ArrowDown") key = "Down";
  else if (code === "Minus") key = "-";
  else if (code === "Equal") key = "Equal";
  else if (code === "Comma") key = ",";
  else if (code === "Period") key = ".";
  else if (code === "Semicolon") key = ";";
  else if (code === "Quote") key = "'";
  else if (code === "BracketLeft") key = "[";
  else if (code === "BracketRight") key = "]";
  else if (code === "Backquote") key = "`";
  else if (code === "Enter") key = "Enter";
  else if (code === "Tab") key = "Tab";

  if (!key) return null; // 只按了修饰键，继续等待
  if (mods.length === 0 && !/^F([1-9]|1[0-2])$/.test(key)) return "";
  return [...mods, key].join("+");
}

function onRecordKeydown(e: KeyboardEvent) {
  if (!recordingShortcut) return;
  e.preventDefault();
  e.stopPropagation();
  if (e.key === "Escape") {
    stopRecording();
    return;
  }
  const accelerator = acceleratorFromEvent(e);
  if (accelerator === null) return;
  stopRecording();
  if (accelerator === "") {
    showShortcutError("请至少包含一个修饰键（Fn / ⌘ / ⌥ / ⌃ / ⇧），或单独使用 F1–F12。");
    return;
  }
  saveShortcut(accelerator);
}

function saveShortcut(accelerator: string) {
  invoke<UiState>("update_shortcut", { shortcut: accelerator })
    .then((state) => applyState(state))
    .catch((error) => showShortcutError(error instanceof Error ? error.message : String(error)));
}

type ShortcutCaptureEvent = { accelerator: string; error: string };

function onNativeShortcutCaptured(payload: ShortcutCaptureEvent) {
  if (!recordingShortcut) return;
  stopRecording();
  if (payload.error) {
    showShortcutError(payload.error);
    return;
  }
  if (payload.accelerator) saveShortcut(payload.accelerator);
}

/* ---------------- 命令调用 ---------------- */

async function refreshState() {
  const state = await invoke<UiState>("get_state");
  applyState(state);
}

async function toggleDictation() {
  try {
    const state = await invoke<UiState>("toggle_dictation");
    applyState(state);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    renderDictationError(message);
  }
}

async function openExternalLink(url: string) {
  try {
    await invoke("open_external_url", { url });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    window.alert(`无法打开链接：${message}`);
  }
}

async function saveVolcSettings() {
  const apiKey = (($("#volc-api-key") as HTMLInputElement | null)?.value ?? "").trim();
  const resourceId = (($("#volc-resource-id") as HTMLInputElement | null)?.value ?? "").trim();
  const boostingTableId = (($("#volc-table-id") as HTMLInputElement | null)?.value ?? "").trim();
  if (!apiKey) {
    setVolcConnectionStatus("#volc-connection-status", "请先粘贴豆包语音 API Key。", "warn");
    return;
  }
  const button = $("#save-volc-btn") as HTMLButtonElement | null;
  const originalLabel = button?.textContent || "验证并保存";
  if (button) {
    button.disabled = true;
    button.textContent = "验证中…";
  }
  setVolcConnectionStatus(
    "#volc-connection-status",
    "正在验证服务并安全保存 API Key…",
    "testing",
  );
  try {
    const state = await invoke<UiState>("save_volc_settings", {
      apiKey,
      resourceId,
      boostingTableId,
    });
    credentialEditorOpen = false;
    applyState(state);
    const input = $("#volc-api-key") as HTMLInputElement | null;
    if (input) input.value = "";
    setVolcConnectionStatus("#volc-connection-status", "✓ 验证通过并已安全保存。", "ok");
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    await refreshState().catch((stateError) =>
      console.error("refresh failed saved credential status failed", stateError),
    );
    setVolcConnectionStatus("#volc-connection-status", message, "warn");
  } finally {
    if (button) {
      button.disabled = false;
      button.textContent = originalLabel;
    }
  }
}

function setVolcCredentialEditor(open: boolean) {
  credentialEditorOpen = open;
  clearVolcConnectionStatus("#volc-connection-status");
  if (!open) {
    const input = $("#volc-api-key") as HTMLInputElement | null;
    if (input) input.value = "";
  }
  if (currentState) applyState(currentState);
  if (open) {
    window.setTimeout(() => ($("#volc-api-key") as HTMLInputElement | null)?.focus(), 0);
  }
}

async function removeVolcApiKey() {
  if (!window.confirm("移除豆包语音 API Key 后将无法继续听写。确定移除吗？")) return;
  try {
    const state = await invoke<UiState>("remove_volc_api_key");
    credentialEditorOpen = true;
    applyState(state);
    setVolcConnectionStatus(
      "#volc-connection-status",
      "API Key 已移除，请重新配置后再开始听写。",
      "warn",
    );
  } catch (error) {
    setVolcConnectionStatus(
      "#volc-connection-status",
      error instanceof Error ? error.message : String(error),
      "warn",
    );
  }
}

type VolcConnectionTestOptions = {
  inputSelector: string;
  resourceSelector?: string;
  statusSelector: string;
  buttonSelector: string;
};

function setVolcConnectionStatus(
  selector: string,
  message: string,
  kind: "testing" | "ok" | "warn",
) {
  const status = $(selector);
  if (!status) return;
  status.className = `connection-test-status ${kind}`;
  status.textContent = message;
}

function clearVolcConnectionStatus(selector: string) {
  const status = $(selector);
  if (!status) return;
  status.className = "connection-test-status";
  status.textContent = "";
}

async function testVolcConnection(options: VolcConnectionTestOptions): Promise<boolean> {
  const input = $(options.inputSelector) as HTMLInputElement | null;
  const resourceInput = options.resourceSelector
    ? ($(options.resourceSelector) as HTMLInputElement | null)
    : null;
  const button = $(options.buttonSelector) as HTMLButtonElement | null;
  const apiKey = (input?.value ?? "").trim();
  const resourceId = (resourceInput?.value || currentState?.volcResourceId || "").trim();
  const originalLabel = button?.textContent || "验证";

  if (button) {
    button.disabled = true;
    button.textContent = "验证中…";
  }
  setVolcConnectionStatus(
    options.statusSelector,
    "正在验证豆包语音服务连接…",
    "testing",
  );

  try {
    const message = await invoke<string>("test_volc_connection", { apiKey, resourceId });
    await refreshState().catch((error) => console.error("refresh credential status failed", error));
    setVolcConnectionStatus(options.statusSelector, `✓ ${message}`, "ok");
    return true;
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    await refreshState().catch((stateError) =>
      console.error("refresh failed credential status failed", stateError),
    );
    setVolcConnectionStatus(options.statusSelector, message, "warn");
    return false;
  } finally {
    if (button) {
      button.disabled = false;
      button.textContent = originalLabel;
    }
  }
}

async function onGainChanged() {
  const gain = Number(($("#gain-db") as HTMLInputElement | null)?.value || 0);
  const gainLabel = $("#gain-db-label");
  if (gainLabel) gainLabel.textContent = gain > 0 ? `增强 +${gain} dB` : "默认";
  try {
    const state = await invoke<UiState>("set_input_gain", { gainDb: gain });
    applyState(state);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    window.alert(message);
    if (currentState) applyState(currentState);
  }
}

async function setHistoryTextSize(size: string) {
  try {
    const state = await invoke<UiState>("set_history_text_size", { size });
    applyState(state);
  } catch (error) {
    console.error("set history text size failed", error);
    if (currentState) applyState(currentState);
  }
}

async function saveRecognitionOptions() {
  const semanticPunctuationEnabled =
    ($("#semantic-punctuation") as HTMLInputElement | null)?.checked ?? true;
  const semanticSmoothingEnabled =
    ($("#semantic-smoothing") as HTMLInputElement | null)?.checked ?? false;
  const maxSentenceSilenceMs = Number(
    ($("#silence-ms") as HTMLSelectElement | null)?.value || 0,
  );
  try {
    const state = await invoke<UiState>("update_recognition_options", {
      semanticPunctuationEnabled,
      semanticSmoothingEnabled,
      maxSentenceSilenceMs,
    });
    applyState(state);
  } catch (error) {
    window.alert(error instanceof Error ? error.message : String(error));
    if (currentState) applyState(currentState);
  }
}

async function onMicChanged() {
  const deviceId = ($("#mic-select") as HTMLSelectElement | null)?.value ?? "";
  try {
    const state = await invoke<UiState>("set_input_device", { deviceId });
    applyState(state);
  } catch (error) {
    console.error("set mic failed", error);
  }
}

async function startMicTest() {
  const onboardingVisible = !$("#onboarding")?.classList.contains("hidden");
  if (onboardingVisible) {
    setOnboardingMicHint(
      onboardingMicNeedsManualRecovery
        ? "正在重新检测麦克风权限…"
        : "正在请求麦克风权限，请在系统弹窗中选择「允许」。",
      "warn",
    );
    // Paint the permission explanation before the synchronous native capture
    // call can display a blocking macOS TCC dialog.
    await waitForNextPaint();
  }
  try {
    const state = await invoke<UiState>("start_mic_test");
    onboardingMicVerified = true;
    onboardingMicNeedsManualRecovery = false;
    applyState(state);
    if (onboardingVisible) {
      setOnboardingMicHint("✓ 麦克风已授权。对着麦克风说话，确认音量条会跳动。", "ok");
    }
  } catch (error) {
    onboardingMicVerified = false;
    console.error("mic test failed", error);
    const message = error instanceof Error ? error.message : String(error);
    try {
      const permissions = await invoke<PermissionStatus>("get_permissions");
      onboardingMicNeedsManualRecovery = !permissions.microphone;
    } catch (permissionError) {
      console.error("read microphone permission after test failure failed", permissionError);
      onboardingMicNeedsManualRecovery =
        message.includes("已被拒绝") ||
        message.includes("系统限制") ||
        message.includes("尚未授权");
    }
    if (onboardingMicNeedsManualRecovery) {
      try {
        applyState(await invoke<UiState>("get_state"));
        onboardingMicNeedsManualRecovery = true;
      } catch (stateError) {
        console.error("refresh state after microphone denial failed", stateError);
      }
    }
    const onboardingNowVisible = !$("#onboarding")?.classList.contains("hidden");
    if (onboardingVisible || onboardingNowVisible) {
      updateOnboardingNextLabel();
      if (currentState) applyMicUi(currentState);
      setOnboardingMicHint(
        onboardingMicNeedsManualRecovery
          ? `未能使用麦克风：${message} 请点击「打开麦克风设置」，手动开启 JackVoice 后再回来重新检测。`
          : `未能使用麦克风：${message}`,
        "warn",
      );
    } else {
      window.alert(`未能使用麦克风：${message}`);
    }
  }
}

function setOnboardingMicHint(message: string, tone: "ok" | "warn") {
  const hint = $("#ob-mic-hint");
  if (!hint) return;
  hint.className = `ob-hint ${tone}`;
  hint.textContent = message;
}

function waitForNextPaint(): Promise<void> {
  return new Promise((resolve) => {
    window.requestAnimationFrame(() => window.requestAnimationFrame(() => resolve()));
  });
}

async function stopMicTest() {
  try {
    const state = await invoke<UiState>("stop_mic_test");
    applyState(state);
  } catch (error) {
    console.error("stop mic test failed", error);
  }
}

async function setLaunchAtLogin(enabled: boolean) {
  try {
    const state = await invoke<UiState>("set_launch_at_login", { enabled });
    applyState(state);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    window.alert(message);
    const toggle = $("#autostart-toggle") as HTMLInputElement | null;
    if (toggle) toggle.checked = !enabled;
  }
}

async function setMuteSystemAudioDuringDictation(enabled: boolean) {
  try {
    const state = await invoke<UiState>("set_mute_system_audio_during_dictation", { enabled });
    applyState(state);
    showToast(enabled ? "已开启听写时自动静音" : "已关闭听写时自动静音", "success");
  } catch (error) {
    window.alert(error instanceof Error ? error.message : String(error));
    if (currentState) applyState(currentState);
  }
}

/* ---------------- 首次启动引导（Onboarding） ---------------- */

const OB_STEP_COUNT = 6;
let obStep = 0;
let obBound = false;

function updateOnboardingNextLabel() {
  const next = $("#ob-next") as HTMLButtonElement | null;
  if (!next) return;
  next.disabled =
    (obStep === 1 && !onboardingMicVerified) ||
    (obStep === 2 && !onboardingAccessibilityGranted);
  if (obStep === OB_STEP_COUNT - 1) {
    next.textContent = "开始使用";
  } else if (
    obStep === 4 &&
    (currentState?.volcCredentialStatus !== "verified" || !!currentState.volcCredentialWarning)
  ) {
    next.textContent = "暂时跳过";
  } else {
    next.textContent = "下一步";
  }
}

function renderOnboardingCompletion(state: UiState) {
  const title = $("#ob-complete-title");
  const copy = $("#ob-complete-copy");
  const detail = $("#ob-complete-detail");
  if (state.volcCredentialStatus === "verified" && !state.volcCredentialWarning) {
    if (title) title.textContent = "准备就绪";
    if (copy) copy.innerHTML = "现在按下 <kbd class=\"shortcut\">⌥ + Space</kbd> 开始语音输入；说完再按一次结束，文字会自动插入。";
    if (detail) detail.textContent = "麦克风、实时识别和自动插入均已配置。";
  } else {
    if (title) title.textContent = "本地录音已就绪";
    if (copy) copy.textContent = "你现在可以使用快捷键开始本地录音，录音会永久保存在本机。";
    if (detail) detail.textContent = state.hasVolcApiKey
      ? "API Key 已配置但尚未验证可用；验证通过后即可生成并自动插入识别文字。"
      : "麦克风和辅助功能已配置；稍后连接 API Key 即可生成并自动插入识别文字。";
  }
}

function goObStep(n: number) {
  obStep = Math.max(0, Math.min(OB_STEP_COUNT - 1, n));
  document.querySelectorAll<HTMLElement>(".ob-pane").forEach((pane) => {
    pane.classList.toggle("active", pane.dataset.pane === String(obStep));
  });
  document.querySelectorAll<HTMLElement>(".ob-dot").forEach((dot) => {
    const idx = Number(dot.dataset.dot);
    dot.classList.toggle("active", idx === obStep);
    dot.classList.toggle("done", idx < obStep);
  });
  const label = $("#ob-step-label");
  if (label) label.textContent = `${obStep + 1} / ${OB_STEP_COUNT}`;
  const back = $("#ob-back") as HTMLButtonElement | null;
  if (back) back.disabled = obStep === 0;
  updateOnboardingNextLabel();
  if (obStep === 2) void refreshAccessibilityStatus();
  if (obStep === 5 && currentState) renderOnboardingCompletion(currentState);
}

/** Lazily populate the microphone dropdown in the regular settings view.
 *  Onboarding deliberately waits for the explicit "测试麦克风" action. */
async function refreshMicDevices() {
  try {
    const state = await invoke<UiState>("list_input_devices");
    applyState(state);
  } catch (error) {
    console.error("list input devices failed", error);
  }
}

async function refreshAccessibilityStatus() {
  try {
    const granted = await invoke<boolean>("check_accessibility_permission");
    renderAccessibilityStatus(granted);
  } catch (error) {
    console.error("check permissions failed", error);
  }
}

function renderAccessibilityStatus(granted: boolean) {
  onboardingAccessibilityGranted = granted;
  updateOnboardingNextLabel();
  const status = $("#ob-access-status");
  if (!status) return;
  if (granted) {
    status.className = "ob-status ok";
    status.textContent = "✓ 辅助功能已授权，识别结果可自动插入。";
  } else {
    status.className = "ob-status warn";
    status.textContent =
      "尚未授权：辅助功能是必需权限。请在系统设置中打开 JackVoice 的开关，然后点「我已开启，重新检测」。";
  }
}

function bindOnboarding() {
  if (obBound) return;
  obBound = true;

  $("#ob-next")?.addEventListener("click", async () => {
    if (obStep === 1) {
      if (!onboardingMicVerified) {
        setOnboardingMicHint("请先点击「测试麦克风」，确认 JackVoice 能够正常录音后再继续。", "warn");
        return;
      }
      if (currentState?.micTesting) await stopMicTest();
    }
    if (obStep === 2 && !onboardingAccessibilityGranted) {
      const status = $("#ob-access-status");
      if (status) {
        status.className = "ob-status warn";
        status.textContent =
          "请先开启辅助功能权限，并点击「我已开启，重新检测」；此权限是使用 JackVoice 的必要条件。";
      }
      return;
    }
    if (obStep === 3) {
      const confirmed = ($("#ob-privacy-confirm") as HTMLInputElement | null)?.checked ?? false;
      const error = $("#ob-privacy-error");
      error?.classList.toggle("hidden", confirmed);
      if (!confirmed) return;
    }
    if (obStep < OB_STEP_COUNT - 1) {
      goObStep(obStep + 1);
      return;
    }
    if (currentState?.micTesting) await stopMicTest();
    try {
      const privacyConfirmed =
        ($("#ob-privacy-confirm") as HTMLInputElement | null)?.checked ?? false;
      const state = await invoke<UiState>("complete_onboarding", { privacyConfirmed });
      $("#ob-complete-error")?.classList.add("hidden");
      applyState(state);
    } catch (error) {
      console.error("complete onboarding failed", error);
      const message = error instanceof Error ? error.message : String(error);
      const errorElement = $("#ob-complete-error");
      if (errorElement) {
        errorElement.textContent = message;
        errorElement.classList.remove("hidden");
      }
    }
  });

  $("#ob-back")?.addEventListener("click", () => goObStep(obStep - 1));

  $("#ob-mic-test-btn")?.addEventListener("click", () => void startMicTest());
  $("#ob-mic-recheck-btn")?.addEventListener("click", () => void startMicTest());
  $("#ob-mic-settings-btn")?.addEventListener("click", async () => {
    try {
      await invoke("open_permission_settings", { permission: "microphone" });
      setOnboardingMicHint(
        "已打开系统设置。请在「隐私与安全性 → 麦克风」中开启 JackVoice，然后回来点击「我已开启，重新检测」。",
        "warn",
      );
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      setOnboardingMicHint(`无法打开麦克风设置：${message}`, "warn");
    }
  });
  $("#ob-mic-stop-btn")?.addEventListener("click", () => void stopMicTest());
  $("#ob-mic-select")?.addEventListener("change", () => {
    const deviceId = ($("#ob-mic-select") as HTMLSelectElement | null)?.value ?? "";
    void invoke<UiState>("set_input_device", { deviceId })
      .then(applyState)
      .catch(console.error);
  });

  $("#ob-access-btn")?.addEventListener("click", async () => {
    try {
      const granted = await invoke<boolean>("request_accessibility_permission");
      renderAccessibilityStatus(granted);
      const status = $("#ob-access-status");
      if (status && !granted) {
        status.className = "ob-status warn";
        status.textContent =
          "已打开系统设置。请找到 JackVoice 并打开开关，然后回来点「我已开启，重新检测」。";
      }
    } catch (error) {
      console.error("request accessibility failed", error);
      const status = $("#ob-access-status");
      if (status) {
        status.className = "ob-status warn";
        status.textContent = `无法打开辅助功能设置：${error instanceof Error ? error.message : String(error)}`;
      }
    }
  });

  $("#ob-access-check")?.addEventListener("click", () => void refreshAccessibilityStatus());

  $("#ob-privacy-confirm")?.addEventListener("change", () => {
    $("#ob-privacy-error")?.classList.add("hidden");
  });

  $("#ob-save-volc-key")?.addEventListener("click", async () => {
    const input = $("#ob-volc-api-key") as HTMLInputElement | null;
    const value = (input?.value ?? "").trim();
    if (!value) {
      setVolcConnectionStatus("#ob-key-test-status", "请先粘贴豆包语音 API Key。", "warn");
      return;
    }
    const button = $("#ob-save-volc-key") as HTMLButtonElement | null;
    const originalLabel = button?.textContent || "验证并保存";
    if (button) {
      button.disabled = true;
      button.textContent = "验证中…";
    }
    setVolcConnectionStatus(
      "#ob-key-test-status",
      "正在验证服务并安全保存 API Key…",
      "testing",
    );
    try {
      const state = await invoke<UiState>("save_volc_settings", {
        apiKey: value,
        resourceId: currentState?.volcResourceId || "volc.seedasr.sauc.duration",
        boostingTableId: currentState?.volcBoostingTableId || "",
      });
      onboardingCredentialEditorOpen = false;
      applyState(state);
      if (input) input.value = "";
      setVolcConnectionStatus("#ob-key-test-status", "✓ 验证通过并已安全保存。", "ok");
    } catch (error) {
      console.error("save onboarding volc key failed", error);
      await refreshState().catch((stateError) =>
        console.error("refresh onboarding credential status failed", stateError),
      );
      const desc = $("#ob-key-desc");
      if (desc) {
        desc.textContent = error instanceof Error ? error.message : String(error);
        desc.classList.add("warn");
      }
      setVolcConnectionStatus(
        "#ob-key-test-status",
        error instanceof Error ? error.message : String(error),
        "warn",
      );
    } finally {
      if (button) {
        button.disabled = false;
        button.textContent = originalLabel;
      }
    }
  });

  $("#ob-test-volc-key")?.addEventListener("click", () => {
    void testVolcConnection({
      inputSelector: "#ob-volc-api-key",
      statusSelector: "#ob-key-test-status",
      buttonSelector: "#ob-test-volc-key",
    });
  });

  $("#ob-edit-volc-key")?.addEventListener("click", () => {
    onboardingCredentialEditorOpen = true;
    clearVolcConnectionStatus("#ob-key-test-status");
    if (currentState) applyState(currentState);
    window.setTimeout(() => ($("#ob-volc-api-key") as HTMLInputElement | null)?.focus(), 0);
  });
  $("#ob-cancel-volc-key")?.addEventListener("click", () => {
    onboardingCredentialEditorOpen = false;
    clearVolcConnectionStatus("#ob-key-test-status");
    const input = $("#ob-volc-api-key") as HTMLInputElement | null;
    if (input) input.value = "";
    if (currentState) applyState(currentState);
  });
}

/* ---------------- 绑定 ---------------- */

function bindNav() {
  document.querySelectorAll<HTMLElement>(".nav-item[data-view]").forEach((item) => {
    item.addEventListener("click", () => {
      document.querySelectorAll(".nav-item[data-view]").forEach((n) => n.classList.remove("active"));
      item.classList.add("active");
      const view = item.getAttribute("data-view");
      $("#view-home")?.classList.toggle("active", view === "home");
      $("#view-dict")?.classList.toggle("active", view === "dict");
    });
  });
}

function bindSettingsModal() {
  $("#open-settings")?.addEventListener("click", openSettings);
  $("#close-settings")?.addEventListener("click", closeSettings);
  $("#settings-modal")?.addEventListener("click", (e) => {
    if (e.target === e.currentTarget) closeSettings();
  });
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !recordingShortcut) closeSettings();
  });

  document.querySelectorAll<HTMLElement>(".modal-nav-item[data-tab]").forEach((item) => {
    item.addEventListener("click", () => {
      document.querySelectorAll(".modal-nav-item").forEach((n) => n.classList.remove("active"));
      item.classList.add("active");
      const tab = item.getAttribute("data-tab");
      $("#tab-general")?.classList.toggle("active", tab === "general");
      $("#tab-about")?.classList.toggle("active", tab === "about");
    });
  });

  $("#shortcut-btn")?.addEventListener("click", () => {
    if (recordingShortcut) stopRecording();
    else startRecording();
  });
  window.addEventListener("keydown", onRecordKeydown, true);

  $("#autostart-toggle")?.addEventListener("change", (e) => {
    const enabled = (e.target as HTMLInputElement).checked;
    void setLaunchAtLogin(enabled);
  });

  $("#output-mute-toggle")?.addEventListener("change", (e) => {
    const enabled = (e.target as HTMLInputElement).checked;
    void setMuteSystemAudioDuringDictation(enabled);
  });

  $("#history-text-size")?.addEventListener("change", (e) => {
    void setHistoryTextSize((e.target as HTMLSelectElement).value);
  });
  $("#mic-select")?.addEventListener("change", () => void onMicChanged());
  $("#mic-test-btn")?.addEventListener("click", () => void startMicTest());
  $("#mic-test-stop-btn")?.addEventListener("click", () => void stopMicTest());

  $("#gain-db")?.addEventListener("input", () => void onGainChanged());

  $("#semantic-punctuation")?.addEventListener("change", () => {
    void saveRecognitionOptions();
  });
  $("#semantic-smoothing")?.addEventListener("change", () => {
    void saveRecognitionOptions();
  });
  $("#silence-ms")?.addEventListener("change", () => {
    void saveRecognitionOptions();
  });

  $("#edit-volc-btn")?.addEventListener("click", () => setVolcCredentialEditor(true));
  $("#cancel-volc-btn")?.addEventListener("click", () => setVolcCredentialEditor(false));
  $("#remove-volc-btn")?.addEventListener("click", () => void removeVolcApiKey());
  $("#save-volc-btn")?.addEventListener("click", () => void saveVolcSettings());
  $("#test-volc-btn")?.addEventListener("click", () => {
    void testVolcConnection({
      inputSelector: "#volc-api-key",
      resourceSelector: "#volc-resource-id",
      statusSelector: "#volc-connection-status",
      buttonSelector: "#test-volc-btn",
    });
  });
}

function bindExternalLinks() {
  document.querySelectorAll<HTMLAnchorElement>("a[data-external-link]").forEach((link) => {
    link.addEventListener("click", (event) => {
      event.preventDefault();
      void openExternalLink(link.href);
    });
  });
}

function bindHome() {
  $("#toggle-btn")?.addEventListener("click", () => {
    void toggleDictation();
  });
  $("#dictation-notice-settings")?.addEventListener("click", openSettings);
  $("#history-search")?.addEventListener("input", (event) => {
    historyQuery = (event.target as HTMLInputElement).value;
    renderHistory(historyRecords);
  });

  const detail = $("#history-detail") as HTMLDialogElement | null;
  detail?.addEventListener("close", () => {
    stopHistoryPlayback();
    selectedHistoryId = null;
    document.querySelectorAll<HTMLElement>("[data-history-row]").forEach((row) => {
      row.classList.remove("active");
    });
  });
  detail?.addEventListener("click", (event) => {
    const rect = detail.getBoundingClientRect();
    if (event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom) {
      detail.close();
    }
  });
}

function bindDict() {
  $("#hotwords-input")?.addEventListener("input", syncEditorButtons);
  $("#hotwords-save")?.addEventListener("click", () => void saveHotwordsFromEditor());
  $("#hotwords-sync-volc")?.addEventListener("click", () => void syncVolcHotwordTable());
  $("#replacements-input")?.addEventListener("input", syncReplacementEditorButtons);
  $("#replacements-save")?.addEventListener("click", () => void saveReplacementsFromEditor());
  $("#replacements-clear")?.addEventListener("click", () => {
    const textarea = $("#replacements-input") as HTMLTextAreaElement | null;
    if (textarea) textarea.value = "";
    syncReplacementEditorButtons();
  });
  $("#replacements-add")?.addEventListener("click", () => {
    const textarea = $("#replacements-input") as HTMLTextAreaElement | null;
    if (!textarea) return;
    textarea.focus();
    if (textarea.value && !textarea.value.endsWith("\n")) textarea.value += "\n";
    textarea.value += "|";
    // 光标移到 | 前，方便先写识别结果
    const pos = textarea.value.lastIndexOf("|");
    textarea.setSelectionRange(pos, pos);
    syncReplacementEditorButtons();
  });
  $("#replacements-search")?.addEventListener("input", (e) => {
    replacementFilter = (e.target as HTMLInputElement).value;
    renderReplacements();
  });
  $("#hotwords-clear")?.addEventListener("click", () => {
    const textarea = $("#hotwords-input") as HTMLTextAreaElement | null;
    if (textarea) textarea.value = "";
    syncEditorButtons();
  });
  $("#hotwords-search")?.addEventListener("input", (e) => {
    hotwordFilter = (e.target as HTMLInputElement).value;
    renderHotwords();
  });
  $("#hotwords-import")?.addEventListener("click", () => {
    ($("#hotwords-file") as HTMLInputElement | null)?.click();
  });
  $("#hotwords-file")?.addEventListener("change", async (e) => {
    const input = e.target as HTMLInputElement;
    const file = input.files?.[0];
    if (!file) return;
    try {
      await importHotwordsFromFile(file);
    } catch (error) {
      console.error("import hotwords failed", error);
      window.alert(`导入失败：${error instanceof Error ? error.message : String(error)}`);
    } finally {
      input.value = "";
    }
  });
}

window.addEventListener("DOMContentLoaded", async () => {
  const version = import.meta.env.VITE_JACKVOICE_VERSION || "0.1.1";
  const buildId = import.meta.env.VITE_JACKVOICE_BUILD_ID || "development";
  const buildVersion = $("#app-build-version");
  if (buildVersion) buildVersion.textContent = `v${version} · build ${buildId}`;

  await listen<ShortcutCaptureEvent>("jackvoice://shortcut-captured", (event) =>
    onNativeShortcutCaptured(event.payload),
  );
  ensureSegments();
  bindNav();
  bindHome();
  bindDict();
  bindSettingsModal();
  bindOnboarding();
  bindExternalLinks();

  await refreshState();
  await Promise.all([refreshHistory(), refreshHotwords(), refreshReplacements()]);

  await listen<UiState>("jackvoice://state", (event) => applyState(event.payload));
  await listen<number>("jackvoice://level", (event) => {
    if (currentState?.micTesting) {
      renderLevel(event.payload || 0);
      renderLevel(event.payload || 0, $("#ob-level-segments"));
    }
  });
  await listen("jackvoice://history", () => void refreshHistory());
});
