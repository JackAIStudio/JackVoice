import { listen } from "@tauri-apps/api/event";
import {
  MEMOS_CHANGED_EVENT,
  addMemo,
  createMemoItem,
  loadMemos,
  showUndoBar,
  type MemoItem,
} from "./memos-shared";

const listEl = () => document.querySelector<HTMLElement>("#memos-list");
const countEl = () => document.querySelector<HTMLElement>("#memos-count");
const inputEl = () => document.querySelector<HTMLTextAreaElement>("#memo-input");
const submitBtn = () => document.querySelector<HTMLButtonElement>("#memo-submit");

/** 已完成区是否展开。面板是短命浮窗，这个状态不值得持久化。 */
let showCompleted = false;
/** 折叠切换要重渲染，但手上只有一份最新数据，留着复用。 */
let currentMemos: MemoItem[] = [];

const EMPTY_STATE = `
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

/**
 * 已完成区。做过的条目不是垃圾，只是不该继续占着待办的位置，
 * 所以折叠保留而不是清掉。
 */
function completedToggle(count: number): HTMLElement {
  const toggle = document.createElement("button");
  toggle.type = "button";
  toggle.className = `completed-toggle${showCompleted ? " open" : ""}`;
  toggle.innerHTML = `
    <svg class="chevron" width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <polyline points="9 18 15 12 9 6" />
    </svg>
    <span>已完成 ${count}</span>
  `;
  toggle.addEventListener("click", () => {
    showCompleted = !showCompleted;
    renderMemos(currentMemos);
  });
  return toggle;
}

function renderMemos(memos: MemoItem[]) {
  const container = listEl();
  if (!container) return;
  currentMemos = memos;

  const active = memos.filter((memo) => !memo.completed);
  const completed = memos.filter((memo) => memo.completed);

  const countBadge = countEl();
  if (countBadge) {
    countBadge.textContent = String(active.length);
    countBadge.classList.toggle("zero", active.length === 0);
  }

  if (memos.length === 0) {
    container.innerHTML = EMPTY_STATE;
    return;
  }

  const events = {
    onUpdated: (updated: MemoItem[]) => renderMemos(updated),
    onDeleted: (memo: MemoItem) => showUndoBar(memo, (updated) => renderMemos(updated)),
  };

  container.innerHTML = "";
  for (const memo of active) {
    container.appendChild(createMemoItem(memo, events));
  }

  if (completed.length > 0) {
    container.appendChild(completedToggle(completed.length));
    if (showCompleted) {
      for (const memo of completed) {
        container.appendChild(createMemoItem(memo, events));
      }
    }
  }
}

async function refresh() {
  renderMemos(await loadMemos());
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
    await addMemo(text, "manual");
    resetComposer();
    await refresh();
  } catch (err) {
    console.error("add memo failed", err);
  }
  input.focus();
}

window.addEventListener("DOMContentLoaded", async () => {
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

  await refresh();
  autoGrow();
  syncSubmitState();

  await listen<MemoItem[]>(MEMOS_CHANGED_EVENT, (event) => {
    renderMemos(event.payload);
  });
});
