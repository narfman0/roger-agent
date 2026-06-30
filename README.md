# roger

Matrix-native AI orchestrator written in Rust. Named after the Star Wars Episode I
battle droids.

Roger lives in your Matrix rooms and answers messages there — by text or voice —
keeping a per-room memory that survives restarts. Beyond plain chat, it can route a
room to different models and hand harder work to an agentic coding assistant that
edits files and runs commands in a real project, reporting progress back into the
conversation.

## What it does

- **Conversational, per room.** Each room has its own history and persona, and can
  be pointed at a different model profile. Roger can require an @-mention or respond
  to everything, per room.
- **Voice in.** Audio messages are transcribed (Whisper-compatible) and answered
  like any other message.
- **Many model backends.** A room's profile can target a fast local model, a cloud
  model, or an agentic CLI coding assistant — with automatic fallback to the next
  backend when one is unavailable.
- **Web-aware.** The model can search the web and read pages when it needs current
  information.
- **Agentic coding.** For heavier work, roger spawns a coding agent in a real
  project directory; the model chooses which known project to work in. The agent
  runs its own tools (read, edit, shell) and roger streams its progress into the
  room.
- **Sync, async, or auto.** Quick answers come back inline. Long jobs run in the
  background with a live-updating message, so the room stays usable; "auto" starts
  inline and promotes to the background if it runs long. Background jobs can be
  listed and cancelled from chat.

## How it works

Roger syncs with a Matrix homeserver and watches an allowlist of rooms. An incoming
message resolves to a model profile (a primary backend plus ordered fallbacks),
which may be a normal chat model or an agentic subprocess. The whole response —
generation, streaming, persistence, and final rendering — runs as one background
task that roger either waits on or detaches, depending on the room's comms mode.

Roger's own state (its Matrix identity, encryption store, conversation history, and
logs) lives in a directory under your home, kept separate from any project the
agent works in. That separation keeps the agent's working directory loosely coupled
to roger and means pointing it at a project never exposes roger's credentials.

Configuration is split so secrets stay off version control, and most behaviour
(personas, per-room settings, model routing) reloads live without dropping the
Matrix session.

## Setup & configuration

See [docs/architecture.md](docs/architecture.md) for the architecture, the
configuration model, and operational details (deployment, hot-reload, logging).
