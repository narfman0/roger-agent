# Design: context injection + autocompaction

For roger as a long-running service. Two features, one shared concern (the system
prompt's size vs. the history budget):

1. **Injection** — layer persistent operating instructions and durable memory into
   the system prompt.
2. **Compaction** — when a room's history grows past a token threshold, summarize
   the old turns and distill durable facts into memory, so context stays bounded and
   nothing important is silently dropped.

Grounding (current code): the system message today is just the per-room-or-global
`system_prompt`; `history_token_budget(estimate_tokens(system_prompt))` sizes the
history window off it, so **anything injected into the system prompt automatically
shrinks the history window** — injected memory can't overflow context. `HistoryStore`
has `append`/`load`/`windowed_by_tokens`/`clear` but no rewrite. State lives in
`~/.roger`; config hot-reloads on SIGHUP.

## Locked decisions

- **Memory is two-tier, both auto-distilled by compaction:**
  - **Global** — one small, high-signal shared file, injected into every room.
  - **Per-room** — a more detailed file per room, injected only into that room.
- **Compaction trigger: size threshold only** (no nightly job).
- **Operating instructions are layered:** a global `operating.md` plus an optional
  per-room override (appended after the global).

## Injection

Assemble the system message as, in order:

1. Base persona — existing global/per-room `system_prompt` (unchanged).
2. Operating instructions — global `operating.md` + per-room override if set.
3. `## Memory (global)` — the global memory file.
4. `## Memory (this room)` — the room's memory file.

A new `assemble_system_prompt(...)` builds this; the result flows through the
existing budget/window path untouched. Operating files (committed config) are cached
in `ReloadableState` and refreshed on SIGHUP; memory files (mutable state) are read
on demand each turn so the latest distilled memory is always used (small local files).

**Config**

```toml
[context]
operating_file = "config/operating.md"   # global; optional. Per-room override via
                                          # [rooms.*].operating_file (appended).

[memory]
enabled = true
global_file = "~/.roger/memory/global.md" # per-room lives at ~/.roger/memory/rooms/<room>.md
max_global_tokens = 1500                  # re-summarize the file when it exceeds this
max_room_tokens  = 3000
```

Files: operating instructions in `config/` (committed, editable, hot-reload); memory
under `~/.roger/memory/` (mutable, written by compaction, gitignored via `~/.roger`).

## Compaction

**Trigger:** after a turn's assistant reply is appended, if the room's history token
count exceeds `compaction.trigger_tokens`, run compaction for that room as a
**detached task** (never blocks the response). Guarded by a per-room lock (below).

**Procedure** (`src/compaction.rs`):
1. Load full history; if under threshold, return.
2. Split: keep the last `keep_recent_turns` verbatim; the rest is `old`.
3. One LLM call on the compaction `profile` (a cheap/fast one), given `old` plus the
   current global + room memory, returning three parts: a **conversation summary**,
   **room-specific durable facts**, and **broadly-useful facts** (for global). Parse
   by fixed section headers (robust, no tool-calling needed).
4. `HistoryStore::rewrite(room, [summary_message] + recent)` — the summary is a
   `system` message prefixed `[Earlier conversation summary]`, kept at the head so
   windowing always includes it.
5. Append room facts → per-room memory; broadly-useful facts → global memory.
6. If a memory file exceeds its `max_*_tokens` cap, re-summarize that file in place
   (memory self-compaction — bounds unbounded growth without a nightly job).

**Config**

```toml
[compaction]
enabled = true
trigger_tokens = 6000     # compact a room when its history exceeds this
keep_recent_turns = 8     # preserved verbatim
profile = "fast"          # LLM used to summarize + distill
```

## Cross-cutting

- **Per-room history lock.** Compaction does a read-modify-write of the whole
  history file while normal turns append; add a per-room async mutex (a
  `Mutex<HashMap<room, Arc<Mutex<()>>>>` or similar) held around history mutations
  (append + rewrite). This also fixes a pre-existing benign append race.
- **`/clear` vs memory.** `/clear` wipes conversation history but leaves memory
  (durable, survives clears). Add `/forget` to clear this room's memory (and
  `/forget global` for the shared file).
- **Budget interaction.** Injected operating + memory reduce the history window
  automatically; the memory caps keep that bounded. No separate accounting needed.
- **Not nightly.** Compaction is purely reactive to size. (A nightly sweep can be
  added later as a background task like `reload_on_sighup` if desired.)

## Build plan (commits on master)

1. **Injection** — `[context]` + `[memory]` config; `assemble_system_prompt`;
   operating-file caching in `ReloadableState` + SIGHUP; on-demand memory reads;
   wire into `handle_message` prompt assembly. (Memory files may not exist yet →
   empty sections.)
2. **History rewrite + lock** — `HistoryStore::rewrite`, per-room mutation lock,
   `token_count(room)` helper.
3. **Compaction** — `src/compaction.rs` (summarize + distill), the size trigger in
   `run_response_job`, `[compaction]` config, memory self-compaction on cap.
4. **Memory store + commands** — `src/memory.rs` (global + per-room read/append/
   rewrite with caps); `/forget`; `/status` shows memory sizes.

## Key files
`src/config.rs` ([context]/[memory]/[compaction]); `src/matrix/handler.rs` (assembly,
trigger, /forget); `src/history.rs` (rewrite, lock, token_count); new
`src/compaction.rs`, `src/memory.rs`; `config/operating.md` (new, committed);
`docs/architecture.md`.

## Open sub-decisions (reasonable defaults chosen; flag if you disagree)
- Per-room operating override **appends to** the global (set global empty to replace).
- Global memory receives facts distilled from **every** room's compaction — kept
  small by `max_global_tokens` self-compaction. If cross-room bleed is unwanted,
  restrict global distillation to an allowlisted room.
- Summary/distill uses one parsed multi-section completion (no structured-output
  tool) for backend portability.
