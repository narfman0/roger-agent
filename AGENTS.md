# AGENTS.md

Rust Matrix bot that responds to messages in allowlisted rooms using configurable LLM backends.

## Key source files

| File | Purpose |
|------|---------|
| `src/main.rs` | Entry point, wires all components together |
| `src/config.rs` | Config loading: profiles.toml + backends.<HOST_ROLE>.toml + .env |
| `src/llm.rs` | LLM client, OpenAI-compatible chat completions |
| `src/history.rs` | Per-room conversation history, JSON-backed on disk |
| `src/audio.rs` | Speaches/Whisper audio transcription client |
| `src/matrix/client.rs` | Matrix client build + session persistence |
| `src/matrix/handler.rs` | Event handlers: invite, message, ack+edit flow |

## Build and test

```bash
cargo build
cargo test
```

Requires: Rust 2021 edition, matrix-sdk 0.8, tokio.

## Running locally

Copy `.env.example` (or see README) and `config/backends.example.toml` to `config/backends.local.toml`. Then:

```bash
RUST_LOG=roger=info ./target/debug/roger
```

State is stored in `roger_session/`: SQLite crypto store, `session.json` (Matrix tokens), `history/` (per-room JSON).

## Config

- `config/profiles.toml` — committed, defines LLM profiles and routing
- `config/backends.<HOST_ROLE>.toml` — **gitignored**, backend URLs + api_key_env names
- `.env` — **gitignored**, Matrix credentials + GATEWAY_VKEY

Never commit `.env` or `backends.*.toml` (except `backends.example.toml`).

## Message edit flow

1. Typing indicator sent
2. "Working on it…" message sent immediately (saves the event ID)
3. LLM called with last 20 messages of room history
4. Ack message edited in-place via `m.replace` with final reply

## Adding a new backend kind

Add a variant to `BackendKind` in `src/config.rs` (kebab-case serde name), then handle it in the dispatch logic.
