import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";

type MemoItem = {
  id: string;
  text: string;
  createdAtMs: number;
  completed: boolean;
};

const listEl = () => document.querySelector<HTMLElement>("#memos-list");
const countEl = () => document.querySelector<HTMLElement>("#memos-count");
const clearBtn = () => document.querySelector<HTMLButtonElement>("#clear-btn");

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
    meta.textContent = formatTime(memo.createdAtMs);

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

window.addEventListener("DOMContentLoaded", async () => {
  clearBtn()?.addEventListener("click", async () => {
    try {
      const updated = await invoke<MemoItem[]>("clear_completed_memos");
      renderMemos(updated);
    } catch (err) {
      console.error("clear completed memos failed", err);
    }
  });

  await loadMemos();

  await listen<MemoItem[]>("jackvoice://memos-changed", (event) => {
    renderMemos(event.payload);
  });
});
