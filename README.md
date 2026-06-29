# roger

Matrix-native AI agent written in Rust. Named after the Star Wars Episode I battle droids.

Responds to messages in configured rooms, supports text and voice (audio transcription via Whisper), and maintains per-room conversation history across restarts.

## Features

- Matrix E2EE with persistent session (no re-login on restart)
- Configurable LLM backends: local (LM Studio / Ollama) or cloud (via LiteLLM proxy)
- Per-room conversation history, persisted to disk
- Audio transcription via Speaches (Whisper-compatible)
- Typing indicator + immediate ack message, edited in-place when response is ready
- Room allowlist with per-room mention requirements

## Setup

### 1. Configure backends

```
cp config/backends.example.toml config/backends.local.toml
# Edit backends.local.toml with your URLs and model names
```

### 2. Create .env

```
MATRIX_HOMESERVER=http://192.168.1.11:8008
MATRIX_USER=@roger:your.server
MATRIX_PASSWORD=your-password
ROOM_ALLOWLIST=!roomid1:server,!roomid2:server
HOST_ROLE=local
GATEWAY_VKEY=your-litellm-virtual-key
SPEACHES_URL=http://192.168.1.11:8000
```

### 3. Build and run

```
cargo build --release
./target/release/roger
```

Logs go to stderr. Run with `RUST_LOG=roger=debug` for verbose output.

## Configuration

- `config/profiles.toml` — LLM profiles (committed, no secrets)
- `config/backends.<HOST_ROLE>.toml` — backend URLs and API key env var names (gitignored)
- `.env` — secrets: Matrix credentials, API keys (gitignored)

## Architecture

See [docs/architecture.md](docs/architecture.md).
