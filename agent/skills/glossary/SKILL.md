---
name: jackvoice-glossary
description: 当用户要校正转写、字幕、专有名词写法，或需要 JackVoice 本机热词和替换词偏好时使用。Use when correcting ASR, transcripts, subtitles, or proper nouns with the user's local JackVoice glossary.
---

# JackVoice 词库

On DeepSeek Harness, call the tools as `mcp__jackvoice__<name>` (for example `mcp__jackvoice__get_glossary`). Short names like `get_glossary` still identify the same tools.

Use the JackVoice MCP tools to read the local hotword list and replacement rules. This Skill does not modify the glossary.

If the JackVoice tools are unavailable, read `references/connection.md` and help the user connect the local `jackvoice-mcp` sidecar.

1. Call `get_status`. If `storeAvailable` is false, the glossary files are missing; continue without them and say so.
2. Call `get_glossary`. Prefer this over separately calling `get_hotwords` and `get_replacements`.
   - `replacements` are known A→B rules, including identity locks (`from == to`) that protect longer phrases.
   - `hotwords` are terms the user actually says. English entries may be recognition forms with spaces/punctuation stripped.
   - `canonicalTerms` are the preferred final spellings: replacement `to` values, plus hotwords that are not a replacement `from`.
3. For any source text being corrected, call `apply_replacements` with that text. Use the returned `rewritten` as the deterministic baseline. Do not reimplement the rules in the prompt; longest-match and phrase locks live in this tool.
4. After deterministic replacement, use `canonicalTerms` / remaining hotwords to judge leftover homophones and near-matches. Keep the correction faithful to what was spoken; do not invent content.
5. If a hotword equals some replacement `from`, the displayed form is that rule's `to`.
6. If the glossary is large, pass `query` instead of dumping unused terms into the prompt.

Do not read JackVoice history, recordings, or API keys. Do not claim the glossary is complete or that recognition quality is guaranteed.
