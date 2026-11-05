import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import {
  MEMOS_CHANGED_EVENT,
  addMemo,
  buildPlainText,
  copyText,
  createMemoItem,
  loadMemos,
  showUndoBar,
  type MemoItem,
} from "./memos-shared";

/** 待办同步回执。与 `memos_sync::MemoSyncReport` 对应。 */
type MemoSyncReport = {
  pulled: number;
  pushed: number;
  skipped: number;
  syncedAt: string;
};

type MemoSyncStatus = {
  hasCredentials: boolean;
  key: string;
  lastSyncedAt: string | null;
  lastError: string | null;
  deletedCount: number;
};

const $ = <T extends HTMLElement>(selector: string) => document.querySelector<T>(selector);

/** 已完成区是否展开。主窗口活得久，切走再回来保持原状。 */
let showCompleted = false;
let query = "";
let currentMemos: MemoItem[] = [];
/**
 * 勾出来准备批量复制的条目。左侧那个方框管「这事做完了」，
 * 这个管「这几条我要一起拿走」——两个语义分开，各用各的手势。
 */
const selectedIds = new Set<string>();

const EMPTY_ALL = `
  <div class="todos-empty">
    <strong>还没有待办</strong>
    <span>听写时点亮置顶，或在上面直接添加。</span>
  </div>
`;

const EMPTY_SEARCH = `
  <div class="todos-empty">
    <strong>没有找到相关待办</strong>
    <span>换一个关键词再试试。</span>
  </div>
`;

function matches(memo: MemoItem, keyword: string): boolean {
  return !keyword || memo.text.toLocaleLowerCase().includes(keyword);
}

function partition(): { active: MemoItem[]; completed: MemoItem[]; activeTotal: number } {
  const keyword = query.trim().toLocaleLowerCase();
  const active: MemoItem[] = [];
  const completed: MemoItem[] = [];
  let activeTotal = 0;
  for (const memo of currentMemos) {
    const hit = matches(memo, keyword);
    if (memo.completed) {
      if (hit) completed.push(memo);
    } else {
      activeTotal += 1;
      if (hit) active.push(memo);
    }
  }
  return { active, completed, activeTotal };
}

/**
 * 丢掉已经不存在、或已经被标成完成的选中项：
 * 一条待办勾完成后，就不该继续占着批量复制的名额。
 */
function pruneSelection() {
  const selectable = new Set(
    currentMemos.filter((memo) => !memo.completed).map((memo) => memo.id),
  );
  for (const id of Array.from(selectedIds)) {
    if (!selectable.has(id)) selectedIds.delete(id);
  }
}

/** 复制的目标：挑了就复制挑的，没挑就是当前筛选出的全部未完成项。 */
function copyTargets(): MemoItem[] {
  const { active } = partition();
  if (selectedIds.size === 0) return active;
  return active.filter((memo) => selectedIds.has(memo.id));
}

function toggleSelect(id: string) {
  if (selectedIds.has(id)) selectedIds.delete(id);
  else selectedIds.add(id);
  render();
}

/**
 * 已完成区。做过的条目不是垃圾，只是不该继续占着待办的位置，
 * 所以折叠保留而不是清掉。
 */
function completedToggle(count: number): HTMLElement {
  const toggle = document.createElement("button");
  toggle.type = "button";
  toggle.className = `completed-toggle${showCompleted ? " open" : ""}`;
  toggle.innerHTML = `
    <svg class="chevron" width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <polyline points="9 18 15 12 9 6" />
    </svg>
    <span>已完成 ${count}</span>
  `;
  toggle.addEventListener("click", () => {
    showCompleted = !showCompleted;
    render();
  });
  return toggle;
}

function render() {
  const list = $("#todos-list");
  if (!list) return;
  pruneSelection();
  const { active, completed, activeTotal } = partition();

  const count = $("#todos-count");
  if (count) {
    count.textContent = query.trim() ? `${active.length} / ${activeTotal} 项` : `${activeTotal} 项`;
  }

  // 按钮上直接写清会复制几条：挑了就报挑中的数，没挑就是全部未完成。
  // 范围始终可见，省得按下去才知道拿走了什么。
  const targets = copyTargets();
  const copyButton = $("#todos-copy") as HTMLButtonElement | null;
  if (copyButton) {
    copyButton.disabled = targets.length === 0;
    copyButton.title = selectedIds.size > 0 ? "复制挑中的待办" : "复制全部未完成待办";
    const label = copyButton.querySelector("span");
    if (label) {
      label.textContent =
        selectedIds.size > 0 ? `复制选中 ${targets.length} 项` : `复制全部 ${targets.length} 项`;
    }
  }

  list.innerHTML = "";

  if (currentMemos.length === 0) {
    list.innerHTML = EMPTY_ALL;
    return;
  }

  if (active.length === 0 && completed.length === 0) {
    list.innerHTML = EMPTY_SEARCH;
    return;
  }

  const events = {
    onUpdated: (updated: MemoItem[]) => {
      currentMemos = updated;
      render();
    },
    onDeleted: (memo: MemoItem) =>
      showUndoBar(memo, (updated) => {
        currentMemos = updated;
        render();
      }),
    // 点整行 = 挑中它，不进复制；单条复制走悬停出来的复制图标。
    onRowClick: (memo: MemoItem, element: HTMLElement) => {
      if (memo.completed) {
        void copyText(memo.text, element);
        return;
      }
      toggleSelect(memo.id);
    },
  };

  for (const memo of active) {
    list.appendChild(createMemoItem(memo, { ...events, selected: selectedIds.has(memo.id) }));
  }

  if (completed.length > 0) {
    list.appendChild(completedToggle(completed.length));
    if (showCompleted) {
      for (const memo of completed) {
        list.appendChild(createMemoItem(memo, events));
      }
    }
  }
}

/** 复制挑中的条目；一条没挑就是当前筛选出的全部未完成项。搜索框就是范围选择器。 */
async function copyAll() {
  const button = $("#todos-copy");
  if (!button) return;
  const targets = copyTargets();
  if (targets.length === 0) return;
  await copyText(buildPlainText(targets), button);
}

function autoGrow() {
  const input = $("#todos-input") as HTMLTextAreaElement | null;
  if (!input) return;
  input.style.height = "auto";
  // 待办视图还没切过来时（display: none）scrollHeight 是 0。
  // 这时候留 auto，让 CSS 的 min-height 撑住，别把输入框压成一条线。
  if (input.scrollHeight > 0) {
    input.style.height = `${Math.min(input.scrollHeight, 120)}px`;
  }
}

function syncSubmit() {
  const button = $("#todos-submit") as HTMLButtonElement | null;
  const input = $("#todos-input") as HTMLTextAreaElement | null;
  if (button) button.disabled = !input?.value.trim();
}

/**
 * 提交输入框内容，整段作为一条待办——包括里面所有的换行。
 * 成功才清空，失败时保留原文让用户能重试。
 */
async function submitInput() {
  const input = $("#todos-input") as HTMLTextAreaElement | null;
  const text = input?.value.trim();
  if (!input || !text) return;
  try {
    await addMemo(text, "manual");
    input.value = "";
    autoGrow();
    syncSubmit();
    currentMemos = await loadMemos();
    render();
  } catch (err) {
    console.error("add memo failed", err);
  }
  input.focus();
}

/**
 * 待办面板底部的同步状态。
 *
 * 只在**配置过 TOS** 时才显示。没配的用户看到一行「未配置」只会困惑——
 * 他要的不是这个功能，是「待办能用」。
 */
async function refreshSyncState() {
  const el = $("#todos-sync-state");
  const button = $("#todos-sync") as HTMLButtonElement | null;
  if (!el) return;
  try {
    const status = await invoke<MemoSyncStatus>("get_memos_sync_status");
    if (!status.hasCredentials) {
      el.hidden = true;
      if (button) button.hidden = true;
      return;
    }
    el.hidden = false;
    if (button) button.hidden = false;
    const at = status.lastSyncedAt;
    const error = status.lastError;
    if (error) {
      el.textContent = `上次同步失败：${error}`;
      el.dataset.state = "error";
    } else if (!at) {
      el.textContent = "已配置云端，等待首次同步";
      el.dataset.state = "idle";
    } else {
      el.textContent = `上次同步 ${formatRelative(at)}`;
      el.dataset.state = "ok";
    }
  } catch (error) {
    console.warn("read memos sync status failed", error);
    el.hidden = true;
    if (button) button.hidden = true;
  }
}

/** 把 ISO 时间说成「3 分钟前」。只给同步状态用，别的地方有 formatTime。 */
function formatRelative(iso: string): string {
  const at = Date.parse(iso);
  if (Number.isNaN(at)) return iso;
  const diff = Date.now() - at;
  if (diff < 60_000) return "刚刚";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} 分钟前`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)} 小时前`;
  return `${Math.floor(diff / 86_400_000)} 天前`;
}

/** 面板内的轻提示。和 main.ts 的 showToast 用同一个元素，样式一致。 */
let todoToastTimer: number | undefined;
function todoToast(message: string, kind: "success" | "error") {
  const toast = document.querySelector<HTMLElement>("#app-toast");
  if (!toast) return;
  if (todoToastTimer) window.clearTimeout(todoToastTimer);
  toast.textContent = message;
  toast.className = `app-toast ${kind}`;
  todoToastTimer = window.setTimeout(() => {
    toast.classList.add("hidden");
    todoToastTimer = undefined;
  }, 1800);
}

/** 手动同步：从手机拉一次。失败要说话——这是用户主动点的。 */
async function syncFromPhone() {
  const button = $("#todos-sync") as HTMLButtonElement | null;
  if (button) button.disabled = true;
  try {
    const report = await invoke<MemoSyncReport>("sync_memos");
    // 后端在 pulled > 0 时已经广播过事件，列表会自己刷新；
    // 但用户点的是按钮，得给一句明确的回执。
    if (report.pulled === 0 && report.pushed === 0) {
      todoToast("已是最新，手机那边没有新待办。", "success");
    } else {
      const parts: string[] = [];
      if (report.pulled > 0) parts.push(`拉回 ${report.pulled} 条`);
      if (report.pushed > 0) parts.push(`推上去 ${report.pushed} 条`);
      todoToast(`同步完成：${parts.join("，")}。`, "success");
    }
    currentMemos = await loadMemos();
    render();
    await refreshSyncState();
  } catch (error) {
    todoToast(`同步失败：${error instanceof Error ? error.message : String(error)}`, "error");
    await refreshSyncState();
  } finally {
    if (button) button.disabled = false;
  }
}

export async function initTodos() {  $("#todos-copy")?.addEventListener("click", () => void copyAll());
  $("#todos-submit")?.addEventListener("click", () => void submitInput());
  $("#todos-sync")?.addEventListener("click", () => void syncFromPhone());

  const input = $("#todos-input") as HTMLTextAreaElement | null;
  input?.addEventListener("input", () => {
    syncSubmit();
    autoGrow();
  });
  input?.addEventListener("keydown", (event) => {
    // 回车提交，Shift+回车换行。
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      void submitInput();
    } else if (event.key === "Escape") {
      input.value = "";
      autoGrow();
      syncSubmit();
    }
  });

  const search = $("#todos-search") as HTMLInputElement | null;
  search?.addEventListener("input", () => {
    query = search.value;
    render();
  });

  currentMemos = await loadMemos();
  render();
  autoGrow();
  syncSubmit();
  void refreshSyncState();

  await listen<MemoItem[]>(MEMOS_CHANGED_EVENT, (event) => {
    currentMemos = event.payload;
    render();
  });
}
