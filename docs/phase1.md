# Phase 1

## Goals

Get roger to a daily-driveable state: reliable responses with memory, good UX, stable process.

## Tasks

| # | Task | Status |
|---|------|--------|
| Bug | Message editing — send ack then edit in-place via `m.replace` | ✅ Done |
| 1 | Per-room conversation history, persisted to disk | ✅ Done |
| 2 | System prompt / persona for roger | ⬜ Pending |
| 3 | Slash commands: `/help`, `/clear`, `/status` | ⬜ Pending |
| 4 | Graceful LLM error recovery (edit ack with error text) | ✅ Done |
| 5 | Systemd units: roger + lmstudio-server on `ai` machine | ⬜ Pending |

## Completed

### Bug: Message editing
Roger now sends "Working on it…" immediately on receiving a message, then edits that message in-place with the real LLM response via Matrix's `m.replace` relation. No more silent waits or accumulating extra messages.

### Conversation history
Per-room JSON history in `roger_session/history/`. Last 20 messages passed to the LLM as context on every request. Persists across restarts. User and assistant messages both recorded. History is room-scoped (isolated per room).

### Error recovery
LLM errors are edited into the ack message ("Sorry, I hit an error: …") rather than leaving "Working on it…" hanging.

## Pending

### Task 2: System prompt
Add a configurable system prompt (default in `config/profiles.toml` or `config/system_prompt.txt`). Roger needs a persona and date injection. Per-room override via `[rooms."..."] system_prompt`.

### Task 3: Slash commands
- `/help` — list available commands
- `/clear` — wipe conversation history for this room
- `/status` — uptime, model, room count, history size

### Task 5: Systemd units
Two units on the `ai` machine:
- `lmstudio-server.service` — `lms server start --bind 0.0.0.0`, `Restart=on-failure`
- `roger.service` — `./roger`, `Restart=always`, `After=network.target lmstudio-server.service`
