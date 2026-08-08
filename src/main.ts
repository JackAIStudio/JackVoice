import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";

type InputDeviceInfo = { id: string; name: string; isDefault: boolean };

type UiState = {
  phase: string;
  status: string;
  transcript: string;
  hasVolcApiKey: boolean;
  maskedVolcApiKey: string;
  volcResourceId: string;
  volcBoostingTableId: string;
  semanticPunctuationEnabled: boolean;
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
  onboardingCompleted: boolean;
  audioRetention: string;
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
    engine: string;
    hotwords: string[];
    semanticPunctuationEnabled: boolean;
    maxSentenceSilenceMs: number;
    inputGainDb: number;
    inputDeviceId: string;
  };
};

type HistoryStats = {
  sessions: number;
  totalDurationMs: number;
  totalChars: number;
};

type HistoryData = { records: HistoryRecord[]; stats: HistoryStats };

type PermissionStatus = { microphone: boolean; accessibility: boolean };

// 火山引擎热词文本规范：识别形去空格/标点，最长 32 字。
const VOLC_HOTWORD_MAX_CHARS = 32;
// 火山平台热词表上限。
const HOTWORD_LIMIT = 5000;
const VOLC_ENGINE_ID = "volc-seedasr-streaming";
const VOLC_ENGINE_LABEL = "火山引擎 豆包流式语音识别模型 2.0";
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
let historyPlayback: {
  recordId: string;
  audio: HTMLAudioElement;
  objectUrl: string;
  button: HTMLButtonElement;
} | null = null;
let toastTimer: number | undefined;

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

function formatFileSize(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${Math.max(1, Math.round(bytes / 1024))} KB`;
}

function engineLabel(id?: string): string {
  if (!id) return "";
  if (id === VOLC_ENGINE_ID) return VOLC_ENGINE_LABEL;
  return id;
}

function phaseLabel(phase: string): string {
  switch (phase) {
    case "recording":
      return "正在听写";
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

/* ---------------- 状态渲染 ---------------- */

function applyState(state: UiState) {
  currentState = state;

  const kbd = $("#shortcut-kbd");
  if (kbd) kbd.textContent = formatShortcut(state.shortcut || "Alt+Space");

  const pill = $("#phase-pill");
  if (pill) {
    pill.className = `pill ${state.phase || "idle"}`;
    pill.textContent = phaseLabel(state.phase);
    pill.title = state.status;
  }

  const phaseDot = $("#phase-dot");
  if (phaseDot) phaseDot.className = `phase-dot ${state.phase || "idle"}`;
  const console = $(".dictation-console");
  const active = ["recording", "connecting", "finalizing"].includes(state.phase);
  console?.classList.toggle("active", active);
  const consoleTitle = $("#console-title");
  if (consoleTitle) {
    if (state.phase === "recording") consoleTitle.textContent = "正在听写";
    else if (state.phase === "connecting") consoleTitle.textContent = "正在连接识别服务";
    else if (state.phase === "finalizing") consoleTitle.textContent = "正在生成最终结果";
    else if (state.phase === "error") consoleTitle.textContent = "本次听写未能完成";
    else consoleTitle.textContent = "随时开始听写";
  }

  const toggle = $("#toggle-btn") as HTMLButtonElement | null;
  if (toggle) {
    toggle.textContent = active ? "结束听写" : "开始听写";
    toggle.disabled = state.phase === "connecting" || state.phase === "finalizing";
  }

  const autostart = $("#autostart-toggle") as HTMLInputElement | null;
  if (autostart && document.activeElement !== autostart) {
    autostart.checked = !!state.launchAtLogin;
  }

  if (!recordingShortcut) {
    const scBtn = $("#shortcut-btn");
    if (scBtn) scBtn.textContent = formatShortcut(state.shortcut || "Alt+Space");
  }

  const volcMasked = $("#masked-volc-key");
  if (volcMasked) {
    volcMasked.textContent = state.hasVolcApiKey
      ? `已配置：${state.maskedVolcApiKey}`
      : "未配置";
  }
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
      : "需已配置豆包录音识别 APP Key 与热词表 ID";
  }

  const semantic = $("#semantic-punctuation") as HTMLInputElement | null;
  if (semantic && document.activeElement !== semantic) {
    semantic.checked = !!state.semanticPunctuationEnabled;
  }

  const silence = $("#silence-ms") as HTMLInputElement | null;
  if (silence && document.activeElement !== silence) {
    silence.value = String(state.maxSentenceSilenceMs || 1300);
  }

  const gain = $("#gain-db") as HTMLInputElement | null;
  if (gain && document.activeElement !== gain) {
    gain.value = String(state.inputGainDb || 0);
  }
  const gainLabel = $("#gain-db-label");
  if (gainLabel) gainLabel.textContent = `偏移 ${state.inputGainDb || 0} dB`;

  for (const selector of ["#audio-retention", "#ob-audio-retention"]) {
    const retention = $(selector) as HTMLSelectElement | null;
    if (retention && document.activeElement !== retention) {
      retention.value = state.audioRetention || "thirtyDays";
    }
  }

  const ob = $("#onboarding");
  if (ob) ob.classList.toggle("hidden", !!state.onboardingCompleted);

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
      opt.textContent = "未检测到麦克风";
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
  }
  const obStopBtn = $("#ob-mic-stop-btn");
  if (obStopBtn) obStopBtn.classList.toggle("hidden", !state.micTesting);

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
  stopHistoryPlayback();
  historyRecords = records;
  if (!selectedHistoryId || !records.some((record) => record.id === selectedHistoryId)) {
    selectedHistoryId = records[0]?.id ?? null;
  }
  host.innerHTML = "";

  const count = $("#history-count");
  if (count) count.textContent = `${records.length} 条记录`;
  const clear = $("#clear-history") as HTMLButtonElement | null;
  if (clear) clear.disabled = records.length === 0;

  if (records.length === 0) {
    const empty = document.createElement("div");
    empty.className = "empty-hint";
    empty.textContent = "还没有听写记录。\n按下快捷键，说出你的第一段文字。";
    host.appendChild(empty);
    renderHistoryDetail(null);
    return;
  }

  const groups: { key: string; label: string; items: HistoryRecord[] }[] = [];
  for (const record of records) {
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
      const row = document.createElement("button");
      row.type = "button";
      row.className = `history-row${record.id === selectedHistoryId ? " active" : ""}`;
      row.addEventListener("click", () => {
        selectedHistoryId = record.id;
        renderHistory(records);
      });

      const meta = document.createElement("div");
      meta.className = "history-row-meta";
      const time = document.createElement("span");
      time.textContent = timeLabel(new Date(record.finishedAtMs));
      const duration = document.createElement("span");
      duration.textContent = formatClipDuration(record.durationMs);
      meta.append(time, duration);

      const text = document.createElement("div");
      text.className = "history-row-text";
      text.textContent = record.text;
      row.append(meta, text);
      day.appendChild(row);
    }
    host.appendChild(day);
  }
  renderHistoryDetail(records.find((record) => record.id === selectedHistoryId) ?? null);
}

function renderHistoryDetail(record: HistoryRecord | null) {
  const host = $("#history-detail");
  if (!host) return;
  host.innerHTML = "";
  if (!record) {
    const empty = document.createElement("div");
    empty.className = "detail-empty";
    const mark = document.createElement("div");
    mark.className = "detail-empty-mark";
    mark.textContent = "⌁";
    const title = document.createElement("strong");
    title.textContent = "还没有听写记录";
    const hint = document.createElement("span");
    hint.textContent = "完成一次听写后，可以在这里查看、复制和管理结果。";
    empty.append(mark, title, hint);
    host.appendChild(empty);
    return;
  }

  const date = new Date(record.finishedAtMs);
  const head = document.createElement("header");
  head.className = "detail-head";
  const heading = document.createElement("div");
  const title = document.createElement("h2");
  title.textContent = "听写详情";
  const meta = document.createElement("div");
  meta.className = "detail-meta";
  const engine = engineLabel(record.recognition?.engine);
  meta.textContent = [
    `${dayLabel(date)} ${timeLabel(date)}`,
    formatClipDuration(record.durationMs),
    `${record.charCount} 字`,
    engine,
  ].filter(Boolean).join(" · ");
  heading.append(title, meta);

  const actions = document.createElement("div");
  actions.className = "detail-actions";
  const copy = document.createElement("button");
  copy.type = "button";
  copy.className = "detail-action";
  copy.textContent = "复制文字";
  copy.addEventListener("click", () => void copyHistoryText(record.text, copy));
  actions.appendChild(copy);

  if (record.audio) {
    const play = document.createElement("button");
    play.type = "button";
    play.className = "detail-action";
    play.textContent = "播放录音";
    play.addEventListener("click", () => void toggleHistoryPlayback(record.id, play));
    const reveal = document.createElement("button");
    reveal.type = "button";
    reveal.className = "detail-action";
    reveal.textContent = "显示文件";
    reveal.addEventListener("click", () => {
      reveal.disabled = true;
      void invoke("reveal_history_audio", { recordId: record.id })
        .catch((error) => showToast(String(error), "error"))
        .finally(() => (reveal.disabled = false));
    });
    actions.append(play, reveal);
  }

  const remove = document.createElement("button");
  remove.type = "button";
  remove.className = "detail-action danger";
  remove.textContent = "删除";
  remove.addEventListener("click", () => void deleteHistoryRecord(record.id));
  actions.appendChild(remove);
  head.append(heading, actions);

  const transcript = document.createElement("div");
  transcript.className = "detail-transcript";
  transcript.textContent = record.text;
  const audioNote = document.createElement("div");
  audioNote.className = "detail-audio-note";
  audioNote.textContent = record.audio
    ? `本地 WAV · ${record.audio.sampleRateHz / 1000} kHz · ${formatFileSize(record.audio.sizeBytes)}`
    : "本条记录没有保留本地录音";
  host.append(head, transcript, audioNote);
}

async function deleteHistoryRecord(recordId: string) {
  if (!window.confirm("删除这条听写记录及其本地录音？此操作无法撤销。")) return;
  stopHistoryPlayback();
  try {
    const data = await invoke<HistoryData>("delete_history_record", { recordId });
    selectedHistoryId = null;
    renderStats(data.stats);
    renderHistory(data.records);
    showToast("听写记录已删除", "success");
  } catch (error) {
    await refreshHistory();
    showToast(`删除失败：${String(error)}`, "error");
  }
}

async function clearAllHistory() {
  if (historyRecords.length === 0) return;
  if (!window.confirm("清空所有听写文本、录音和旧备份？此操作无法撤销。")) return;
  stopHistoryPlayback();
  try {
    const data = await invoke<HistoryData>("clear_history");
    selectedHistoryId = null;
    renderStats(data.stats);
    renderHistory(data.records);
    showToast("本地听写数据已清空", "success");
  } catch (error) {
    await refreshHistory();
    showToast(`清空失败：${String(error)}`, "error");
  }
}

function stopHistoryPlayback() {
  if (!historyPlayback) return;
  historyPlayback.audio.pause();
  historyPlayback.button.textContent = "播放录音";
  URL.revokeObjectURL(historyPlayback.objectUrl);
  historyPlayback = null;
}

async function toggleHistoryPlayback(recordId: string, button: HTMLButtonElement) {
  if (historyPlayback?.recordId === recordId) {
    if (historyPlayback.audio.paused) {
      try {
        await historyPlayback.audio.play();
        button.textContent = "暂停";
      } catch (error) {
        console.error("resume history audio failed", error);
        stopHistoryPlayback();
      }
    } else {
      historyPlayback.audio.pause();
      button.textContent = "播放录音";
    }
    return;
  }

  stopHistoryPlayback();
  button.disabled = true;
  button.textContent = "加载中…";
  try {
    const wav = await invoke<ArrayBuffer>("get_history_audio", { recordId });
    const objectUrl = URL.createObjectURL(new Blob([wav], { type: "audio/wav" }));
    const audio = new Audio(objectUrl);
    historyPlayback = { recordId, audio, objectUrl, button };
    audio.addEventListener("ended", stopHistoryPlayback, { once: true });
    audio.addEventListener(
      "error",
      () => {
        console.error("play history audio failed", audio.error);
        stopHistoryPlayback();
      },
      { once: true },
    );
    await audio.play();
    button.textContent = "暂停";
  } catch (error) {
    console.error("load history audio failed", error);
    stopHistoryPlayback();
    button.textContent = "播放录音";
  } finally {
    button.disabled = false;
  }
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
  $("#settings-modal")?.classList.remove("hidden");
  void refreshMicDevices();
}

function closeSettings() {
  if (recordingShortcut) stopRecording();
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
  const btn = $("#shortcut-btn");
  if (btn) {
    btn.classList.add("recording");
    btn.textContent = "按下快捷键（Esc 取消）";
  }
}

function stopRecording() {
  recordingShortcut = false;
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
  else if (code === "Equal") key = "+";
  else if (code === "Comma") key = ",";
  else if (code === "Period") key = ".";
  else if (code === "Semicolon") key = ";";
  else if (code === "Quote") key = "'";
  else if (code === "BracketLeft") key = "[";
  else if (code === "BracketRight") key = "]";
  else if (code === "Backquote") key = "`";
  else if (code === "Enter") key = "Return";
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
    showShortcutError("请至少包含一个修饰键（⌘ / ⌥ / ⇧ / ），或单独使用 F1–F12。");
    return;
  }
  invoke<UiState>("update_shortcut", { shortcut: accelerator })
    .then((state) => applyState(state))
    .catch((error) => showShortcutError(error instanceof Error ? error.message : String(error)));
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
    const pill = $("#phase-pill");
    if (pill) {
      pill.className = "pill error";
      pill.textContent = "出错";
      pill.title = message;
    }
  }
}

async function saveVolcSettings() {
  const apiKey = (($("#volc-api-key") as HTMLInputElement | null)?.value ?? "").trim();
  const resourceId = (($("#volc-resource-id") as HTMLInputElement | null)?.value ?? "").trim();
  const boostingTableId = (($("#volc-table-id") as HTMLInputElement | null)?.value ?? "").trim();
  try {
    const state = await invoke<UiState>("save_volc_settings", {
      apiKey,
      resourceId,
      boostingTableId,
    });
    applyState(state);
    const input = $("#volc-api-key") as HTMLInputElement | null;
    if (input) input.value = "";
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    window.alert(message);
    if (currentState) applyState(currentState);
  }
}

async function onGainChanged() {
  const gain = Number(($("#gain-db") as HTMLInputElement | null)?.value || 0);
  const gainLabel = $("#gain-db-label");
  if (gainLabel) gainLabel.textContent = `偏移 ${gain} dB`;
  try {
    const state = await invoke<UiState>("set_input_gain", { gainDb: gain });
    applyState(state);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    window.alert(message);
    if (currentState) applyState(currentState);
  }
}

async function saveOptions() {
  const semantic = ($("#semantic-punctuation") as HTMLInputElement | null)?.checked ?? true;
  const silence = Number(($("#silence-ms") as HTMLInputElement | null)?.value || 1300);
  const state = await invoke<UiState>("update_recognition_options", {
    semanticPunctuationEnabled: semantic,
    maxSentenceSilenceMs: silence,
  });
  applyState(state);
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
  try {
    const state = await invoke<UiState>("start_mic_test");
    applyState(state);
  } catch (error) {
    console.error("mic test failed", error);
  }
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

async function setAudioRetention(retention: string) {
  try {
    const state = await invoke<UiState>("set_audio_retention", { retention });
    applyState(state);
    await refreshHistory();
    showToast("录音保留策略已更新", "success");
  } catch (error) {
    window.alert(error instanceof Error ? error.message : String(error));
    if (currentState) applyState(currentState);
    await refreshHistory();
  }
}

/* ---------------- 首次启动引导（Onboarding） ---------------- */

const OB_STEP_COUNT = 6;
let obStep = 0;
let obBound = false;

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
  const next = $("#ob-next") as HTMLButtonElement | null;
  if (next) next.textContent = obStep === OB_STEP_COUNT - 1 ? "开始使用" : "下一步";
  if (obStep === 1) {
    void refreshMicPermissionHint();
    void refreshMicDevices();
  }
  if (obStep === 2) void refreshAccessibilityStatus();
}

/** Lazily populate the microphone dropdown. The Rust side no longer
 *  enumerates audio devices at startup (that would trigger the macOS mic
 *  permission prompt before the user reaches the mic step). */
async function refreshMicDevices() {
  try {
    const state = await invoke<UiState>("list_input_devices");
    applyState(state);
  } catch (error) {
    console.error("list input devices failed", error);
  }
}

async function refreshMicPermissionHint() {
  try {
    const p = await invoke<PermissionStatus>("check_permissions");
    const hint = $("#ob-mic-hint");
    if (!hint) return;
    if (p.microphone) {
      hint.className = "ob-hint ok";
      hint.textContent = "✓ 麦克风已授权，可以直接测试声音。";
    } else {
      hint.className = "ob-hint warn";
      hint.textContent = "点击「测试麦克风」，系统会弹出授权窗口，请选择「允许」。";
    }
  } catch (error) {
    console.error("check permissions failed", error);
  }
}

async function refreshAccessibilityStatus() {
  try {
    const p = await invoke<PermissionStatus>("check_permissions");
    renderAccessibilityStatus(p.accessibility);
  } catch (error) {
    console.error("check permissions failed", error);
  }
}

function renderAccessibilityStatus(granted: boolean) {
  const status = $("#ob-access-status");
  if (!status) return;
  if (granted) {
    status.className = "ob-status ok";
    status.textContent = "✓ 辅助功能已授权，识别结果可自动插入。";
  } else {
    status.className = "ob-status";
    status.textContent =
      "尚未授权：请在系统设置中打开 JackVoice 的开关，然后点「我已开启，重新检测」。";
  }
}

function bindOnboarding() {
  if (obBound) return;
  obBound = true;

  $("#ob-next")?.addEventListener("click", async () => {
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
      const state = await invoke<UiState>("complete_onboarding");
      applyState(state);
    } catch (error) {
      console.error("complete onboarding failed", error);
    }
  });

  $("#ob-back")?.addEventListener("click", () => goObStep(obStep - 1));

  $("#ob-mic-test-btn")?.addEventListener("click", () => void startMicTest());
  $("#ob-mic-stop-btn")?.addEventListener("click", () => void stopMicTest());
  $("#ob-mic-select")?.addEventListener("change", () => {
    const deviceId = ($("#ob-mic-select") as HTMLSelectElement | null)?.value ?? "";
    void invoke<UiState>("set_input_device", { deviceId })
      .then(applyState)
      .catch(console.error);
  });

  $("#ob-access-btn")?.addEventListener("click", async () => {
    try {
      const p = await invoke<PermissionStatus>("request_accessibility_permission");
      renderAccessibilityStatus(p.accessibility);
      const status = $("#ob-access-status");
      if (status && !p.accessibility) {
        status.className = "ob-status";
        status.textContent =
          "已打开系统设置。请找到 JackVoice 并打开开关，然后回来点「我已开启，重新检测」。";
      }
    } catch (error) {
      console.error("request accessibility failed", error);
    }
  });

  $("#ob-access-check")?.addEventListener("click", () => void refreshAccessibilityStatus());

  $("#ob-autostart")?.addEventListener("change", (e) => {
    const enabled = (e.target as HTMLInputElement).checked;
    void setLaunchAtLogin(enabled);
  });

  $("#ob-audio-retention")?.addEventListener("change", (e) => {
    void setAudioRetention((e.target as HTMLSelectElement).value);
  });

  $("#ob-save-volc-key")?.addEventListener("click", async () => {
    const input = $("#ob-volc-api-key") as HTMLInputElement | null;
    const value = (input?.value ?? "").trim();
    try {
      const state = await invoke<UiState>("save_volc_settings", {
        apiKey: value,
        resourceId: currentState?.volcResourceId || "volc.seedasr.sauc.duration",
        boostingTableId: currentState?.volcBoostingTableId || "",
      });
      applyState(state);
      if (input) input.value = "";
      const desc = $("#ob-key-desc");
      if (desc) {
        desc.textContent = state.hasVolcApiKey
          ? `已配置：${state.maskedVolcApiKey}`
          : "豆包语音新版控制台的 APP Key；也可稍后再到「设置 → 识别」里配置。";
      }
    } catch (error) {
      console.error("save onboarding volc key failed", error);
    }
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

  $("#audio-retention")?.addEventListener("change", (e) => {
    void setAudioRetention((e.target as HTMLSelectElement).value);
  });
  $("#settings-clear-history")?.addEventListener("click", () => void clearAllHistory());

  $("#mic-select")?.addEventListener("change", () => void onMicChanged());
  $("#mic-test-btn")?.addEventListener("click", () => void startMicTest());
  $("#mic-test-stop-btn")?.addEventListener("click", () => void stopMicTest());

  $("#gain-db")?.addEventListener("input", () => void onGainChanged());

  $("#save-volc-btn")?.addEventListener("click", () => void saveVolcSettings());
  $("#save-options-btn")?.addEventListener("click", () => void saveOptions());
}

function bindHome() {
  $("#toggle-btn")?.addEventListener("click", () => void toggleDictation());
  $("#clear-history")?.addEventListener("click", () => void clearAllHistory());
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
  ensureSegments();
  bindNav();
  bindHome();
  bindDict();
  bindSettingsModal();
  bindOnboarding();

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
