# Architecture

## Overview

Roger is a single-process Rust async application using `matrix-sdk` (tokio-based). It syncs with a Matrix homeserver, listens for messages in allowlisted rooms, and responds via LLM.

## Config system

Two-file design keeps secrets off GitHub:

- `config/profiles.toml` — committed. Defines named LLM profiles (`chat`, `fast`, `code`, etc.) and routing rules. References backends by logical name, not URL.
- `config/backends.<HOST_ROLE>.toml` — **gitignored**. Maps logical backend names to real URLs, models, and `api_key_env` variable names. `HOST_ROLE` env var selects which file to load (default: `local`).

API keys are never in config files — only the *name* of the env var holding them.

## Session persistence

On first login, roger saves `roger_session/session.json` (access_token, device_id, user_id). On restart, it restores from this file instead of re-logging in. This prevents a new device ID on every restart, which would conflict with the SQLite E2EE crypto store.

## Conversation history

`HistoryStore` writes one JSON file per room to `roger_session/history/`. Room IDs are sanitized for filesystem safety. Context window is the last 20 messages, passed as the full messages array to the LLM.

History is room-scoped by default. No cross-room sharing (each room has independent context).

## Response UX

For every response:
1. `room.typing_notice(true)` — shows the typing indicator in Matrix clients
2. Send "Working on it…" immediately — user sees activity before LLM responds
3. Call LLM with room history
4. Edit the ack message in-place via `m.replace` (ruma `ReplacementMetadata`) — no extra messages accumulate
5. `room.typing_notice(false)`

## Audio pipeline

1. Matrix sends an `m.audio` event with encrypted media
2. `matrix_sdk::media` downloads and decrypts via `MediaRequestParameters`
3. Raw bytes POST'd to Speaches (`/v1/audio/transcriptions`, model `Systran/faster-whisper-small`)
4. Transcript text fed into normal LLM flow

## LiteLLM proxy

All cloud LLM calls go through a LiteLLM Docker container on `srv:4000`. This:
- Keeps the Anthropic API key on one machine (`srv`) only
- The `ai` machine uses a LiteLLM virtual key (`GATEWAY_VKEY`) with no direct Anthropic access
- Lets backends be swapped without changing roger's config

## Backend kinds

- `open-ai` — standard OpenAI-compatible REST API (LM Studio, Ollama, LiteLLM)
- `claude-code` — reserved for spawning `claude -p` subprocess (not yet implemented)
- `open-code` — reserved for `opencode run` subprocess (future)
