# Agent 词库

JackVoice 可以把本机热词和替换词提供给本地 Agent。应用不必正在运行。当前只读，不会改词库、不会读听写历史、录音或 API Key。

这是 JackVoice 自己的能力，不绑定任何剪辑软件。

## 两份名单

| 名单 | 用途 |
|---|---|
| 热词 | 你常用的专有词。Agent 校正转写时，把听着像、看着像的识别结果往这边靠。 |
| 替换词 | 已知的 A→B。识别结果里出现 `from`，最终写法用 `to`。`from == to` 是短语锁，避免短规则拆坏长短语。 |

英文热词可能是识别形（去空格/标点，如 `Broll`）。最终写法在替换词的 `to`（如 `B roll`）。中文热词常常本身就是最终写法。

校正顺序：

1. 调用 `apply_replacements` 做确定性替换（最长匹配、忽略大小写、短语锁）。
2. 用热词和 `canonicalTerms` 处理剩余的同音、近形。
3. 若某热词等于某条替换的 `from`，展示用 `to`。

## 构建 MCP

在仓库根目录：

```bash
cargo build --locked --release \
  --manifest-path src-tauri/Cargo.toml \
  -p jackvoice-agent-bridge \
  --bin jackvoice-mcp
```

二进制在 `src-tauri/target/release/jackvoice-mcp`。桌面应用的 Cargo 缓存目录与此无关，这个 sidecar 很小，也不依赖 WebRTC。

```bash
src-tauri/target/release/jackvoice-mcp --version
src-tauri/target/release/jackvoice-mcp --health-check
```

可用 `JACKVOICE_SHARED_DATA_DIR` 覆盖词库目录；默认与应用相同，为系统数据目录下的 `com.jackvoice.shared`。

## 接到 Agent

DeepSeek Harness / Codex 等本地 MCP 宿主把 `jackvoice-mcp` 注册为 stdio 服务器，服务名用 `jackvoice`。

Codex 示例：

```bash
codex mcp add jackvoice -- /absolute/path/to/jackvoice-mcp
```

Skill 在 `agent/skills/glossary/`。可以链到 Agent 的 skill 目录：

```bash
ln -sfn /absolute/path/to/JackVoice/agent/skills/glossary \
  ~/.agents/skills/jackvoice-glossary
```

工具：

- `get_status`
- `get_glossary`（一次拿热词、替换词和规范写法，校正时优先用这个）
- `get_hotwords`
- `get_replacements`
- `apply_replacements`

当前版本没有写入工具。词库仍在 JackVoice 词库页维护。
