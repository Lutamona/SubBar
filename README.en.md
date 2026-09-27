# SubBar

**Claude Code subagents stop eating your Claude limit.** SubBar is a macOS menu bar app with a tiny local proxy:
your main Claude Code session stays on your Claude subscription byte-for-byte, while `haiku` subagents with tools
(optionally `sonnet`) run on [OpenCode Go](https://opencode.ai) models (DeepSeek and others). If OpenCode fails,
the request falls back to real Claude, so work never stops. The menu bar also shows limits for Claude,
ChatGPT/Codex, OpenCode Go, Command Code, Devin and any service with a JSON API.

## Install (macOS 13+, Apple Silicon or Intel)

```bash
curl -fsSL https://raw.githubusercontent.com/Lutamona/SubBar/main/scripts/get.sh | bash
```

1. Menu bar icon → **+** → **OpenCode Go** → paste your API key. The proxy starts by itself.
2. Optional status line in Claude Code: `/Applications/SubBar.app/Contents/MacOS/SubBar statusline install`.
3. Run `claude-sub` instead of `claude`. Same login, same flags.

Your Claude OAuth token never goes to OpenCode (covered by an end-to-end test). No telemetry, no servers of ours.
The UI is in Russian; full docs: [README.md](README.md) (RU), agent guide: [docs/FOR-AI.md](docs/FOR-AI.md).

MIT. Not affiliated with Anthropic or OpenCode.
