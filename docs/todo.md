# Phase 1

## Goals

Get roger to a daily-driveable state: reliable responses with memory, good UX, stable process.

## Tasks

| # | Task | Status |
|---|------|--------|
| Bug | Message editing — send ack then edit in-place via `m.replace` | ✅ Done |
| 1 | Per-room conversation history, persisted to disk | ✅ Done |
| 2 | System prompt / persona for roger | ✅ Done |
| 3 | Slash commands: `/help`, `/clear`, `/status` | ✅ Done |
| 4 | Graceful LLM error recovery (edit ack with error text) | ✅ Done |
| 5 | Systemd units: roger + lmstudio-server on `ai` machine | ✅ Done |

## Completed

### Bug: Message editing
Roger now sends "Working on it…" immediately on receiving a message, then edits that message in-place with the real LLM response via Matrix's `m.replace` relation. No more silent waits or accumulating extra messages.

### Conversation history
Per-room JSON history in `roger_session/history/`. Last 20 messages passed to the LLM as context on every request. Persists across restarts. User and assistant messages both recorded. History is room-scoped (isolated per room).

### Error recovery
LLM errors are edited into the ack message ("Sorry, I hit an error: …") rather than leaving "Working on it…" hanging.

### System prompt (#2)
`config/system_prompt.txt` loaded at startup with `{date}` injected. Passed as the first `system` role message in every LLM call. Per-room override planned for a future phase.

### Slash commands (#3)
Commands are intercepted before the LLM — handled inline and edited into the ack message:
- `/help` — lists available commands
- `/clear` — wipes history for the current room
- `/status` — uptime, history count for the room
- Unknown commands get a helpful error pointing to `/help`

### Systemd units (#5)
`deploy/lmstudio-server.service` and `deploy/roger.service` created with install instructions in `deploy/README.md`. Run `systemctl --user enable/start` after copying to `~/.config/systemd/user/`.

## Phase 2 (next)

See the full roadmap in conversation history. Key next items:
- Config hot-reload (`SIGHUP`)
- `/status` model name display
- Per-room system prompt override
- Structured logging with rotation
