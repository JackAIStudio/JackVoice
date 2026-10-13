---
name: jackvoice-memos
description: 读取本机 JackVoice 待办事项——用户说出来的、面板里手打的、从剪贴板收进来的都在一起。用户问「我还有什么没做」「待办里有什么」时用。只读，不改待办。
---

# JackVoice 待办事项

用户没有点名 JackVoice、待办或备忘时，不要调用下面的工具。

On DeepSeek Harness, call the tools as `mcp__jackvoice__<name>` (for example `mcp__jackvoice__get_memos`). Short names like `get_memos` still identify the same tools.

Use the JackVoice MCP tools to read the local todo list. This Skill never adds, completes, or deletes a memo.

If the JackVoice tools are unavailable, the local `jackvoice-mcp` sidecar is not registered. The build and registration steps are the same as for the glossary tools; see `agent/README.md` in the JackVoice source tree. Do not suggest downloading JackVoice from unofficial mirrors.

1. Call `get_status`. If `storeAvailable` is false, the shared data directory is missing — JackVoice has probably never run on this machine. Say so and stop.
2. Call `get_memos`. It returns the pending items by default.
   - Each entry has `text`, `createdAtMs`, `completed`, and `source`.
   - `source` tells you where the item came from, and it is worth surfacing when the distinction matters: `voice` was dictated (the user said it), `manual` was typed in the memo panel, `clipboard` was collected from the clipboard (the user copied it from somewhere else).
   - Pass `includeCompleted: true` only when the user explicitly asks about finished items.
   - Pass `query` to filter by text instead of dumping a long list into the prompt.
3. Answer from the returned `text` values. Do not editorialise, reorder by guessed priority, or invent items that were not returned.
4. `pendingCount` is the number of unfinished memos regardless of `query` and `limit`, so use it when the user asks "how many".

This tool is read-only in this version. If the user asks you to add or tick off a memo, say that the JackVoice panel is where that happens, and do not claim you changed anything.
