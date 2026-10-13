import { invoke } from "@tauri-apps/api/core";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";

export type MemoSource = "voice" | "manual" | "clipboard";

export type MemoItem = {
  id: string;
  text: string;
  createdAtMs: number;
  completed: boolean;
  source: MemoSource;
};

/**
 * 待办变化事件。托盘面板和主窗口是两个独立 webview，共用这一份广播，
 * 任一边改动另一边立刻跟上。
 */
export const MEMOS_CHANGED_EVENT = "jackvoice://memos-changed";

/** 非语音来源在时间旁挂一枚单色细线图标；语音是默认来源，不标注。 */
const SOURCE_ICONS: Record<string, { title: string; svg: string }> = {
  manual: {
    title: "手动输入",
    svg: `<rect x="2" y="6" width="20" height="12" rx="2.5" />
      <path d="M6.4 10.2h.01" /><path d="M10.1 10.2h.01" /><path d="M13.8 10.2h.01" /><path d="M17.5 10.2h.01" />
      <path d="M7.6 13.9h8.8" />`,
  },
  clipboard: {
    title: "来自剪贴板",
    svg: `<rect x="8" y="2" width="8" height="4" rx="1" />
      <path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2" />`,
  },
};

export function sourceBadge(source: MemoSource): HTMLElement | null {
  const icon = SOURCE_ICONS[source];
  if (!icon) return null;
  const badge = document.createElement("span");
  badge.className = "memo-source";
  badge.title = icon.title;
  badge.innerHTML = `
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      ${icon.svg}
    </svg>
  `;
  return badge;
}

export function formatTime(timestamp: number): string {
  const now = Date.now();
  const diff = now - timestamp;
  if (diff < 60 * 1000) return "刚刚";
  if (diff < 60 * 60 * 1000) return `${Math.floor(diff / (60 * 1000))} 分钟前`;

  const date = new Date(timestamp);
  const today = new Date();
  const isToday =
    date.getDate() === today.getDate() &&
    date.getMonth() === today.getMonth() &&
    date.getFullYear() === today.getFullYear();

  const hours = String(date.getHours()).padStart(2, "0");
  const minutes = String(date.getMinutes()).padStart(2, "0");

  if (isToday) return `今天 ${hours}:${minutes}`;
  return `${date.getMonth() + 1}/${date.getDate()} ${hours}:${minutes}`;
}

export async function copyText(text: string, triggerEl: HTMLElement) {
  try {
    await writeText(text);
  } catch {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      // fallback
    }
  }

  // 避免同时存在多个微反馈 toast
  triggerEl.querySelector(".copy-toast")?.remove();
  const toast = document.createElement("div");
  toast.className = "copy-toast";
  toast.innerHTML = `
    <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <polyline points="20 6 9 17 4 12" />
    </svg>
    <span>已复制</span>
  `;
  triggerEl.appendChild(toast);

  window.setTimeout(() => {
    toast.remove();
  }, 750);
}

export async function loadMemos(): Promise<MemoItem[]> {
  try {
    return await invoke<MemoItem[]>("get_memos");
  } catch (err) {
    console.error("load memos failed", err);
    return [];
  }
}

export async function addMemo(text: string, source: MemoSource = "manual"): Promise<MemoItem> {
  return await invoke<MemoItem>("add_memo", { text, source });
}

export async function toggleMemo(id: string): Promise<MemoItem[]> {
  return await invoke<MemoItem[]>("toggle_memo", { id });
}

export async function deleteMemo(id: string): Promise<MemoItem[]> {
  return await invoke<MemoItem[]>("delete_memo", { id });
}

/** 删除后恢复。后端按 createdAtMs 插回原位，撤销不会打乱顺序。 */
export async function restoreMemo(memo: MemoItem): Promise<MemoItem[]> {
  return await invoke<MemoItem[]>("restore_memo", { memo });
}

let undoTimer: number | null = null;

function dismissUndoBar() {
  if (undoTimer !== null) {
    window.clearTimeout(undoTimer);
    undoTimer = null;
  }
  document.querySelector(".undo-bar")?.remove();
}

/**
 * 删除后的撤销条。删除不可恢复，所以每次都留一条几秒钟的后悔窗口。
 * 挂在 body 上而不是列表里——列表随时会被重渲染清空，撤销条得活着。
 */
export function showUndoBar(memo: MemoItem, onRestored: (memos: MemoItem[]) => void) {
  dismissUndoBar();

  const bar = document.createElement("div");
  bar.className = "undo-bar";

  const label = document.createElement("span");
  label.textContent = "已删除";

  const button = document.createElement("button");
  button.type = "button";
  button.textContent = "撤销";
  button.addEventListener("click", async () => {
    try {
      onRestored(await restoreMemo(memo));
    } catch (err) {
      console.error("restore memo failed", err);
    }
    dismissUndoBar();
  });

  bar.append(label, button);
  document.body.appendChild(bar);
  undoTimer = window.setTimeout(dismissUndoBar, 5000);
}

/**
 * 批量复制用的纯文本。
 *
 * 单行条目之间只留一个换行，保持紧凑；只要相邻两侧有任何一边是多行条目，
 * 就补一个空行——否则从剪贴板收进来的整段文字会和相邻待办粘成一条，
 * 复制出去的边界就和用户看到的不一样了。
 */
export function buildPlainText(memos: MemoItem[]): string {
  let out = "";
  memos.forEach((memo, index) => {
    if (index > 0) {
      const previous = memos[index - 1];
      out += previous.text.includes("\n") || memo.text.includes("\n") ? "\n\n" : "\n";
    }
    out += memo.text;
  });
  return out;
}

export type MemoItemEvents = {
  onUpdated: (memos: MemoItem[]) => void;
  /** 删除成功后的回调，用来挂撤销入口。 */
  onDeleted?: (memo: MemoItem) => void;
  /**
   * 点击整行的行为。不传就是「单击复制」——托盘小窗要的就是一眼看到、一把拿走。
   * 主窗口传自己的处理函数，把整行变成批量挑选的选择区，
   * 免得和左边的完成勾选框抢同一个手势。
   */
  onRowClick?: (memo: MemoItem, element: HTMLElement) => void;
  /** 批量挑选时的选中态，只影响样式。 */
  selected?: boolean;
};

/**
 * 渲染单条待办。托盘面板和主窗口共用同一套结构，
 * 各自的 CSS 决定它是深色玻璃还是浅色卡片。
 */
export function createMemoItem(memo: MemoItem, events: MemoItemEvents): HTMLElement {
  const item = document.createElement("div");
  item.className = ["memo-item", memo.completed ? "completed" : "", events.selected ? "selected" : ""]
    .filter(Boolean)
    .join(" ");
  item.dataset.id = memo.id;
  // 已完成的条目不参与批量挑选，点它们就是想拿走内容。
  const rowCopies = !events.onRowClick || memo.completed;
  item.title = rowCopies ? "单击复制内容" : "单击选中";

  const checkbox = document.createElement("div");
  checkbox.className = "memo-checkbox";
  checkbox.title = memo.completed ? "标为未完成" : "标为已完成";
  if (memo.completed) {
    checkbox.innerHTML = `
      <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <polyline points="20 6 9 17 4 12" />
      </svg>
    `;
  }
  checkbox.addEventListener("click", async (e) => {
    e.stopPropagation();
    try {
      events.onUpdated(await toggleMemo(memo.id));
    } catch (err) {
      console.error("toggle memo failed", err);
    }
  });

  const content = document.createElement("div");
  content.className = "memo-content";

  const text = document.createElement("div");
  text.className = "memo-text";
  text.textContent = memo.text;

  const meta = document.createElement("div");
  meta.className = "memo-meta";
  const time = document.createElement("span");
  time.textContent = formatTime(memo.createdAtMs);
  meta.appendChild(time);
  const badge = sourceBadge(memo.source);
  if (badge) meta.appendChild(badge);

  content.append(text, meta);

  const actions = document.createElement("div");
  actions.className = "memo-actions";

  const copyBtn = document.createElement("button");
  copyBtn.className = "memo-action-btn copy-action";
  copyBtn.title = "复制";
  copyBtn.innerHTML = `
    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <rect x="9" y="9" width="13" height="13" rx="2" ry="2" />
      <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />
    </svg>
  `;
  copyBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    void copyText(memo.text, item);
  });

  const delBtn = document.createElement("button");
  delBtn.className = "memo-action-btn delete-action";
  delBtn.title = "删除";
  delBtn.innerHTML = `
    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <line x1="18" y1="6" x2="6" y2="18" />
      <line x1="6" y1="6" x2="18" y2="18" />
    </svg>
  `;
  delBtn.addEventListener("click", async (e) => {
    e.stopPropagation();
    try {
      const updated = await deleteMemo(memo.id);
      events.onUpdated(updated);
      events.onDeleted?.(memo);
    } catch (err) {
      console.error("delete memo failed", err);
    }
  });

  actions.append(copyBtn, delBtn);
  item.append(checkbox, content, actions);

  // 点击整行：托盘小窗直接一键复制，主窗口交给调用方做批量挑选。
  item.addEventListener("click", (e) => {
    const target = e.target as HTMLElement;
    if (target.closest(".memo-checkbox") || target.closest(".memo-action-btn")) {
      return;
    }
    if (events.onRowClick) events.onRowClick(memo, item);
    else void copyText(memo.text, item);
  });

  return item;
}
