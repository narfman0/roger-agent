# roger

Matrix-native AI orchestrator written in Rust. Named after the Star Wars Episode I
battle droids.

Roger lives in your Matrix rooms and answers messages there — by text or voice —
carrying persistent memory and a learnable set of skills across restarts. Beyond
plain chat, it routes each room to different models, delegates to purpose-built
subagents, and hands heavier work to an agentic coding assistant that edits files
and runs commands — safely, on a branch — reporting progress back into the room.

## What it does

- **Conversational, per room.** Each room has its own history and persona, can be
  pointed at a different model profile, and processes its messages in order — a long
  background job never blocks a quick follow-up. @-mention required or not, per room.
- **Voice in.** Audio messages are transcribed (Whisper-compatible) and answered
  like any other message.
- **Many model backends, with fallback.** A room's profile can target a fast local
  model, a cloud model, or an agentic CLI coding assistant — falling back to the next
  backend when one is unavailable.
- **Durable memory + auto-compaction.** Roger keeps a global and a per-room memory,
  distilled automatically as long conversations are summarized, so context stays
  useful and bounded across restarts.
- **Web- and file-aware, extensible via MCP.** The model can search the web, read
  and write files, and use tools from any connected MCP servers.
- **Agentic coding, isolated.** For heavier work, roger runs a coding agent (which
  uses its own read/edit/shell tools) in an isolated git worktree of a real project;
  changes land on a branch for review, so the running system is never edited out from
  under itself.
- **Delegation to subagents.** A model can hand a scoped task to a named subagent —
  its own persona and model — including a coding subagent, and get the result back.
- **Skills it can learn.** Reusable procedures are injected into context and loaded
  on demand; roger can draft new ones from experience, saved for your approval.
- **Sync, async, or auto.** Quick answers come back inline; long jobs run in the
  background with a live-updating message; "auto" starts inline and promotes to the
  background if it runs long. Background jobs can be listed and cancelled from chat.

## How it works

Roger syncs with a Matrix homeserver and watches an allowlist of rooms. Each room is
a serial worker that handles its messages in order. An incoming message resolves to a
model profile (a primary backend plus ordered fallbacks), which may be a normal chat
model or an agentic subprocess. Its system prompt is assembled from a persona,
operating instructions, and durable memory. The whole response — generation,
streaming, persistence, and final rendering — runs as one task the room either waits
on or detaches, depending on the comms mode.

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
