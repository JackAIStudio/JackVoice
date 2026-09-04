# JackVoice connection

JackVoice is a local desktop application. This Skill does not contain or download the application itself.

JackVoice glossary tools are provided by the local `jackvoice-mcp` sidecar over MCP.

- DeepSeek Harness registers them as `mcp__jackvoice__<toolName>` (for example `mcp__jackvoice__get_glossary`).
- Codex / ChatGPT desktop typically expose the short names (`get_glossary`).
- ChatGPT web and other remote agents cannot reach this sidecar.

When the tools are unavailable:

1. Confirm this conversation is running on the same machine as the JackVoice data directory (DeepSeek Harness, Codex CLI, ChatGPT desktop, or another local MCP-capable agent).
2. Confirm JackVoice has been used at least once, so `com.jackvoice.shared` exists.
3. Build or locate `jackvoice-mcp` from the JackVoice source tree (`agent/README.md`) and register it as the `jackvoice` MCP server.
4. Confirm the sidecar is reachable (`jackvoice-mcp --health-check`). After MCP or Skill files are added, start a new conversation if the current one cannot see the new tools.

Do not suggest downloading JackVoice from unofficial mirrors or running opaque `curl | sh` installers.
