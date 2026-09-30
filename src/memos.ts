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

  // 避免同时存在多个 toast
  triggerEl.querySelector(".copy-toast")?.remove();
  const toast = document.createElement("div");
  toast.className = "copy-toast";
  toast.textContent = "已复制 ✓";
  triggerEl.appendChild(toast);

  window.setTimeout(() => {
    toast.remove();
  }, 900);
}

function renderMemos(memos: MemoItem[]) {
  const container = listEl();
  if (!container) return;

  const activeCount = memos.filter((m) => !m.completed).length;
  if (countEl()) {
    countEl()!.textContent = String(activeCount);
  }

  if (memos.length === 0) {
    container.innerHTML = `
      <div class="empty-state">
        <div class="empty-icon">📝</div>
        <div class="empty-title">暂无待办事项</div>
        <div class="empty-desc">听写说话时点亮胶囊上的 📌 图标<br/>内容将自动收入此处</div>
      </div>
    `;
    return;
  }

  container.innerHTML = "";

  for (const memo of memos) {
    const item = document.createElement("div");
    item.className = `memo-item${memo.completed ? " completed" : ""}`;
    item.dataset.id = memo.id;

    // Checkbox
    const checkbox = document.createElement("div");
    checkbox.className = "memo-checkbox";
    checkbox.title = memo.completed ? "标为未完成" : "标为已完成";
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

    // Delete button
    const delBtn = document.createElement("button");
    delBtn.className = "memo-delete";
    delBtn.textContent = "×";
    delBtn.title = "删除";
    delBtn.addEventListener("click", async (e) => {
      e.stopPropagation();
      try {
        const updated = await invoke<MemoItem[]>("delete_memo", { id: memo.id });
        renderMemos(updated);
      } catch (err) {
        console.error("delete memo failed", err);
      }
    });

    // 点击整行直接一键复制
    item.addEventListener("click", (e) => {
      const target = e.target as HTMLElement;
      if (target.closest(".memo-checkbox") || target.closest(".memo-delete")) {
        return;
      }
      void copyText(memo.text, item);
    });

    item.appendChild(checkbox);
    item.appendChild(content);
    item.appendChild(delBtn);

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
