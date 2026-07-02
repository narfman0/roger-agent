# Phase 5 — capability roadmap

Grows roger from a single-purpose Matrix dispatcher toward a more capable agent
harness, without losing what makes it good (lean Rust binary, explicit fallback
chains, hot-reload). Four workstreams: worktree isolation, MCP, named subagents,
and self-improving skills. See `docs/architecture.md` for current design.

Recommended order: **5.0 → 5.1 → 5.2 → 5.3 → 5.4** (safety first, then the shared
refactor, then features that build on it). Each ships as its own commit(s) on
master, green build + tests, per `AGENTS.md`.

## Tasks

| # | Task | Status |
|---|------|--------|
| 5.0 | Dynamic tool registry (shared prerequisite) | ✅ Done |
| 5.1 | Worktree isolation for agentic jobs | ✅ Done |
| 5.2 | MCP client support (finish `rmcp` WIP) | ✅ Done |
| 5.3 | Named / custom-configured subagents | 📋 planned |
| 5.4 | Self-improving skills (author + suggest) | 📋 planned |

---

## 5.0 — Dynamic tool registry (prerequisite)

**Why:** MCP (5.2), subagents (5.3), and skills (5.4) all add tools the HTTP models
can call. Today `tools::tool_definitions()` is a static free fn and `llm.rs`
hardcodes it in the request. Make the advertised tool list dynamic first so the
later workstreams just register into it.

**Design:** `ToolExecutor` owns the tool set. Add `ToolExecutor::tool_definitions()`
(native + later MCP/skill tools) and route unknown names through a registry rather
than the current hardcoded `match`. `LlmClient::chat_with_tools` calls
`executor.tool_definitions()` instead of the free fn (it already takes
`executor: Option<&ToolExecutor>`). Subprocess backends are unaffected (they own
their own tools).

**Steps:** move tool defs onto `ToolExecutor`; thread the executor's list into the
chat request; keep the native tools working; update the `tool_definitions` test.

**Key files:** `src/tools.rs`, `src/llm.rs`.

---

## 5.1 — Worktree isolation for agentic jobs

**Why:** agentic subprocess jobs (claude-code/opencode) run directly in the room's
workdir — a real repo. Concurrent jobs or a bad edit hit the live checkout (this is
what caused the earlier repo rename scare). Run each job in a throwaway git worktree.

**Design:** when a subprocess job's workdir is a git repo, create a per-job worktree
on a fresh branch (`git worktree add <path> -b roger/<room>/<job>`) under
`~/.roger/worktrees/`, point the subprocess cwd there, and on completion report the
branch + a diff summary back to the room. Non-git workdirs fall back to today's
direct-cwd behavior. Auto-prune the worktree if unchanged (like the built-in Agent
tool); otherwise keep the branch for review. Cleanup on cancel (the `kill_on_drop` +
`process_group` path already reaps the process; add `git worktree remove`).

**Steps:**
1. `[worktrees]` config: `enabled`, base dir, branch prefix, cleanup policy.
2. Detect git repo; create/remove worktree around the spawn in `src/subprocess.rs`
   (thread the worktree path like `WORKDIR`).
3. Track the worktree path in the `Workers` job handle; remove on completion/cancel.
4. Post-run: branch name + `git diff --stat` summary to the room.
5. Relax the per-room agentic serialization once jobs are isolated (optional).

**Key files:** `src/subprocess.rs`, `src/config.rs`, `src/workers.rs`,
`src/matrix/handler.rs`.

**Open decisions:** merge-back policy — leave branch for human review (default) vs.
auto-open a PR (`gh`) vs. auto-commit to a target branch. Where worktrees live if
the repo is outside `~` (disk/permissions).

---

## 5.2 — MCP client support (finish the `rmcp` WIP)

**Why:** `rmcp` is already a dep but unused. Let roger's HTTP models call tools from
configured MCP servers, the way they call `web_search`/`read_file` today. (Subprocess
backends get MCP via their own `--mcp-config`; this is for roger's own tool loop.)

**Design:** `[mcp.servers.<name>]` config → `{ command, args, env }` (stdio child
process via `rmcp` `transport-child-process`; SSE/HTTP later). An `McpManager` in
`BotCtx` spawns/connects servers at startup, lists their tools, and keeps clients
alive (reconnect on failure). MCP tools are namespaced `mcp__<server>__<tool>` and
merged into the dynamic registry (5.0). `ToolExecutor::execute` routes `mcp__*`
calls to the right client. Reload rebuilds the manager on SIGHUP.

**Steps:**
1. MCP config parsing.
2. `src/mcp.rs`: `McpManager` — spawn/connect, `list_tools`, `call_tool`, lifecycle.
3. Register MCP tool defs into the 5.0 registry; route execution.
4. Wire into `BotCtx` + startup/reload; `/status` shows connected servers + tool
   counts.

**Key files:** `src/mcp.rs` (new), `src/tools.rs`, `src/config.rs`, `src/main.rs`,
`src/matrix/handler.rs`.

**Open decisions:** transports beyond stdio (SSE/streamable-HTTP?); per-room or
global server enablement; auth passing (env only vs. per-server secrets).

---

## 5.3 — Named / custom-configured subagents

**Why:** delegation and parallel workstreams (cf. openclaw named agents, hermes
subagents). Let the primary model hand a scoped task to a purpose-built agent
(model + persona + tool allowlist) and get the result back.

**Design:** `[agents.<name>]` config → `{ profile | backend, system_prompt |
prompt_file, tools (allowlist), comms }`. Two invocation paths: a `run_subagent(name,
task)` native tool the model calls (result returned as the tool result), and a
`/agent <name> <task>` command. A subagent runs **headless** (reuse the response
pipeline but capture text instead of streaming to Matrix; optional progress line).
Reuse the `Workers` registry for tracking + concurrency; enforce a **spawn-depth cap**
(e.g. 2) to prevent runaway. A subagent may itself be an HTTP profile or a subprocess
(claude-code) — and, combined with 5.1, subprocess subagents run in worktrees.

**Steps:**
1. `[agents.*]` config parsing (+ hot-reload into `ReloadableState`).
2. `src/subagent.rs`: runner — assemble (agent system prompt + task), run via the
   resolved backend, return text; depth guard; Workers tracking.
3. `run_subagent` tool (registered via 5.0) — needs a dispatcher handle to
   `state`/`llms` (task-local like `ROOM_ID`, or an `Arc` in `ToolExecutor`).
4. `/agents` (list) and `/agent <name> <task>` (manual) commands.

**Key files:** `src/subagent.rs` (new), `src/config.rs`, `src/tools.rs`,
`src/matrix/handler.rs`, `src/workers.rs`.

**Open decisions:** how the tool executor reaches the LLM registry (task-local vs.
injected handle) — mirrors the `set_workdir` wiring question. Streaming vs. silent
subagent progress. Whether subagent tool allowlists compose with MCP/skill tools.

---

## 5.4 — Self-improving skills (author + suggest)

**Why:** accumulate reusable procedures (the capability Hermes/openclaw have that
roger lacks) — but with an approval gate, not silent auto-adoption.

**Design:** a **skill** is a Markdown doc (`SKILL.md`: name, description, when-to-use,
steps) — reuse the openclaw/agentskills.io format for portability. Committed skills
live in `config/skills/`; learned ones in `~/.roger/skills/`. The **skill index**
(names + one-line descriptions) is injected into the system prompt (small, always
on); a full skill body is loaded on demand when referenced. **Self-improvement:**
after a substantial task, a `write_skill` tool lets the model draft a SKILL.md;
**suggestions** are surfaced for approval (a compaction hook or `/skills suggest`
proposes candidates distilled from history/memory) rather than adopted silently —
consistent with the "don't silently adopt" principle. Skills complement memory
(facts) by capturing procedures.

**Steps:**
1. `src/skills.rs`: skill store (committed + learned), `SKILL.md` parse, index build.
2. Inject the skill index into the system prompt (extend `assemble_system_prompt`);
   lazy-load bodies on reference.
3. `write_skill` tool (drafts to a pending/approval area).
4. Suggestion path: `/skills suggest` and/or a compaction hook proposing candidates;
   `/skills approve <name>`, `/skills list`, `/skills forget <name>`.

**Key files:** `src/skills.rs` (new), `src/matrix/handler.rs` (injection + commands),
`src/compaction.rs` (suggestion hook), `src/tools.rs` (`write_skill`),
`src/config.rs`.

**Open decisions:** injection budget for the skill index (cap it, like memory);
auto-suggest cadence; whether a `skill-author` subagent (5.3) writes skills; approval
UX (chat commands vs. edit files directly).

---

## Cross-cutting notes

- **Tool surface grows** across 5.2/5.3/5.4 — all register into the 5.0 registry;
  keep the advertised list bounded (token cost) and namespaced.
- **Safety:** 5.1 (worktrees) plus subagent depth caps and the skill approval gate
  are the guardrails; keep `bypassPermissions` scoped to trusted rooms.
- **Deferred earlier, still open:** session-mapped continuity for subprocess
  backends (`--session-id`), tool-step progress markers, a typing keepalive for long
  jobs, and a nightly compaction sweep.
</content>
