# Agent 词库

JackVoice 可以把本机热词和替换词提供给本地 Agent。应用不必正在运行。当前只读，不会改词库、不会读听写历史、录音或 API Key。

这是 JackVoice 自己的能力，不绑定任何剪辑软件。

## 两份名单

| 名单 | 用途 |
|---|---|
| 热词 | 你常用的专有词。Agent 校正转写时，把听着像、看着像的识别结果往这边靠。 |
| 替换词 | 已知的 A→B。识别结果里出现 `from`，最终写法用 `to`。`from == to` 是短语锁，避免短规则拆坏长短语。英文和数字只在整词边界上替换；中文按连续文字匹配。 |

英文热词可能是识别形（去空格/标点，如 `Broll`）。最终写法在替换词的 `to`（如 `B roll`）。中文热词常常本身就是最终写法。

校正顺序：

1. 调用 `apply_replacements` 做确定性替换（最长匹配、忽略大小写、短语锁；英文和数字只在整词边界上替换，中文按连续文字匹配）。
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

二进制在 Cargo 的 `<target_directory>/release/jackvoice-mcp`，不要写死 `src-tauri/target`。设置了 `CARGO_TARGET_DIR`，或 `.cargo/config.toml` 里写了 `build.target-dir` 时，输出不在仓库里。macOS 上若仓库放在 iCloud 的「桌面」或「文稿」目录，target 应改到本地缓存（例如 `$HOME/Library/Caches/JackVoice/dev-cargo-target`），否则构建产物会被云端驱逐。用下面这条命令读实际的 `target_directory`：

```bash
cargo metadata --format-version 1 --no-deps --manifest-path src-tauri/Cargo.toml
```

桌面应用的 Cargo 缓存目录与此无关，这个 sidecar 很小，也不依赖 WebRTC。

```bash
"<target_directory>/release/jackvoice-mcp" --version
"<target_directory>/release/jackvoice-mcp" --health-check
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

**这个 skill 只由人手打 `/jackvoice-glossary` 唤起。** frontmatter 里带一行 `disable-model-invocation: true`：Agent 的技能目录里不出现它，模型不会因为"任务像字幕/转写"就顺手把它吊起来。

别把这行删掉，也别把"什么时候不要用我"写回 description——描述是每次会话都要付费的目录开销，而且超过 500 字会被截断。反触发交给这一行，描述只写"这个 skill 干什么"（`/` 菜单里给人看）。正文里那句"没点名就不要调用"留着，它不占目录开销。

工具：

- `get_status`
- `get_glossary`（一次拿热词、替换词和规范写法，校正时优先用这个）
- `get_hotwords`
- `get_replacements`
- `apply_replacements`

当前版本没有写入工具。词库仍在 JackVoice 词库页维护。
