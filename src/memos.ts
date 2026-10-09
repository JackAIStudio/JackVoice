import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";

type MemoSource = "voice" | "manual" | "clipboard";

type MemoItem = {
  id: string;
  text: string;
  createdAtMs: number;
  completed: boolean;
  source: MemoSource;
};

const listEl = () => document.querySelector<HTMLElement>("#memos-list");
const countEl = () => document.querySelector<HTMLElement>("#memos-count");
const clearBtn = () => document.querySelector<HTMLButtonElement>("#clear-btn");
const inputEl = () => document.querySelector<HTMLTextAreaElement>("#memo-input");
const submitBtn = () => document.querySelector<HTMLButtonElement>("#memo-submit");

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

function sourceBadge(source: MemoSource): HTMLElement | null {
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

function formatTime(timestamp: number): string {
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

async function copyText(text: string, triggerEl: HTMLElement) {
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

function renderMemos(memos: MemoItem[]) {
  const container = listEl();
  if (!container) return;

  const activeCount = memos.filter((m) => !m.completed).length;
  const countBadge = countEl();
  if (countBadge) {
    countBadge.textContent = String(activeCount);
    countBadge.classList.toggle("zero", activeCount === 0);
  }

  const hasCompleted = memos.some((m) => m.completed);
  const clearButton = clearBtn();
  if (clearButton) {
    clearButton.disabled = !hasCompleted;
  }

  if (memos.length === 0) {
    container.innerHTML = `
      <div class="empty-state">
        <svg class="empty-icon" width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2" />
          <rect x="8" y="2" width="8" height="4" rx="1" ry="1" />
          <path d="m9 14 2 2 4-4" />
        </svg>
        <div class="empty-title">暂无待办事项</div>
        <div class="empty-desc">听写时开启置顶，内容将自动收入</div>
      </div>
    `;
    return;
  }

  container.innerHTML = "";

  for (const memo of memos) {
    const item = document.createElement("div");
    item.className = `memo-item${memo.completed ? " completed" : ""}`;
    item.dataset.id = memo.id;
    item.title = "单击复制内容";

    // Checkbox
    const checkbox = document.createElement("div");
    checkbox.className = "memo-checkbox";
    checkbox.title = memo.completed ? "标为未完成" : "标为已完成";
    if (memo.completed) {
      checkbox.innerHTML = `
        <svg width="9" height="9" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <polyline points="20 6 9 17 4 12" />
        </svg>
      `;
    }
    checkbox.addEventListener("click", async (e) => {
      e.stopPropagation();
      try {
        const updated = await invoke<MemoItem[]>("toggle_memo", { id: memo.id });
        renderMemos(updated);
      } catch (err) {
        console.error("toggle memo failed", err);
      }
    });

    // Content (text + meta)
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

    content.appendChild(text);
    content.appendChild(meta);

    // Actions (Copy + Delete)
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
        const updated = await invoke<MemoItem[]>("delete_memo", { id: memo.id });
        renderMemos(updated);
      } catch (err) {
        console.error("delete memo failed", err);
      }
    });

    actions.appendChild(copyBtn);
    actions.appendChild(delBtn);

    // 点击整行直接一键复制
    item.addEventListener("click", (e) => {
      const target = e.target as HTMLElement;
      if (target.closest(".memo-checkbox") || target.closest(".memo-action-btn")) {
        return;
      }
      void copyText(memo.text, item);
    });

    item.appendChild(checkbox);
    item.appendChild(content);
    item.appendChild(actions);

    container.appendChild(item);
  }
}

async function loadMemos() {
  try {
    const memos = await invoke<MemoItem[]>("get_memos");
    renderMemos(memos);
  } catch (err) {
    console.error("load memos failed", err);
  }
}

/** 输入框会随内容长高，最多到 CSS 里的 max-height，再高就内部滚动。 */
function autoGrow() {
  const input = inputEl();
  if (!input) return;
  input.style.height = "auto";
  input.style.height = `${Math.min(input.scrollHeight, 96)}px`;
}

function syncSubmitState() {
  const button = submitBtn();
  if (button) button.disabled = !inputEl()?.value.trim();
}

function resetComposer() {
  const input = inputEl();
  if (input) input.value = "";
  autoGrow();
  syncSubmitState();
}

/**
 * 提交输入框内容，整段作为一条待办——包括里面所有的换行。
 * 成功才清空，失败时保留原文让用户能重试。
 */
async function submitInput() {
  const input = inputEl();
  const text = input?.value.trim();
  if (!input || !text) return;
  try {
    await invoke<MemoItem>("add_memo", { text, source: "manual" });
    resetComposer();
    await loadMemos();
  } catch (err) {
    console.error("add memo failed", err);
  }
  input.focus();
}

window.addEventListener("DOMContentLoaded", async () => {
  clearBtn()?.addEventListener("click", async () => {
    try {
      const updated = await invoke<MemoItem[]>("clear_completed_memos");
      renderMemos(updated);
    } catch (err) {
      console.error("clear completed memos failed", err);
    }
  });

  const input = inputEl();
  // 粘贴一律走浏览器默认行为，内容落在输入框里由用户过目、编辑、再回车。
  // 曾经在这里拦截多行粘贴并直接入库，用户根本没机会看到自己粘了什么。
  input?.addEventListener("input", () => {
    syncSubmitState();
    autoGrow();
  });
  input?.addEventListener("keydown", (event) => {
    // 回车提交，Shift+回车换行。
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      void submitInput();
    } else if (event.key === "Escape") {
      resetComposer();
    }
  });
  submitBtn()?.addEventListener("click", () => void submitInput());

  // 面板是点托盘弹出的，打开就能直接敲字。
  window.addEventListener("focus", () => inputEl()?.focus());

  await loadMemos();
  autoGrow();
  syncSubmitState();

  await listen<MemoItem[]>("jackvoice://memos-changed", (event) => {
    renderMemos(event.payload);
  });
});
