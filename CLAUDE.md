# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Konata_Mirror (镜中此方) is a Tauri v2 desktop AI chat assistant with role-play personas, a long-term memory system, and a floating desktop pet widget. Rust backend + React 19/TypeScript frontend.

## Commands

- 始终使用简体中文回答用户的问题；
- 完成任务后，在回复内容最后加上“任务完成了喵”。

```bash
# Install dependencies
pnpm install

# Development (starts Vite dev server on :1420 + compiles/runs Tauri backend)
pnpm tauri dev

# Build release (NSIS installer + MSI → src-tauri/target/release/bundle/)
pnpm tauri build

# Frontend-only build (type-check + vite bundle)
pnpm build

# Rust linting
cd src-tauri && cargo clippy

# Rust tests
cd src-tauri && cargo test
```

First `tauri dev` compilation takes 3-5 minutes; subsequent launches are fast.

## Architecture

### Two-Window Design

Tauri defines two windows in `src-tauri/tauri.conf.json`:
- **main** (900×680): Full chat interface with sidebar, message list, input
- **float** (220×340, transparent, always-on-top, frameless): Desktop pet with hover-to-reveal bubble

`src/App.tsx` detects the window label via `getCurrentWebviewWindow().label` and renders `FloatingWidget` or the full page router.

### Streaming Event Contract (important)

Both windows load the same bundle and the backend broadcasts with `app.emit`, so **every** streaming event
must carry both ids; the frontend filters on them:

| Event | Payload |
|---|---|
| `stream-chunk` / `stream-thinking-chunk` / `stream-end` | `{ session_id, stream_id, data }` |
| `message-stats` | `{ session_id, stream_id, token_count, thinking_ms, model? }` |
| `stream-error` | `{ session_id, stream_id, message }` |

- `session_id` isolates conversations; `stream_id` isolates concurrent generations
  (both windows normally share the same "today" session).
- The single source of truth for these shapes is `src/types/events.ts`. Do not redeclare them per component.
- `send_message` takes an optional `stream_id` (auto-generated if omitted) and `persist: false`
  for one-shot interactions (poke reactions) that must not touch the database.
- `isStreaming` must always be reset in a `finally` block — never rely on an event arriving.
- A generation's listeners are registered per `stream_id` and are **not** unregistered when the user changes
  session, so every handler must also require the payload's `session_id` to still be the current session
  (`chatStore.sendMessage`'s `isCurrentGeneration`) — otherwise the old session's reply or tool card lands in
  the new one.
- `tool-output-chunk` carries the same `session_id` / `stream_id` / `call_id` and is **UI-only**: it feeds the
  live output box in `ToolCallCard`, never the LLM context (only `ToolOutput::content` is fed back).
- `run_command` is the only `Execute` tool and never uses a shell: consecutive commands go through its `steps`
  array (≤5, each step re-checked by `CommandGuard` before *any* step runs), output trimming goes through
  `max_output_lines`. `tools.call_timeout_secs` is the per-call budget (default 180 s since the v3 config
  migration — first builds used to blow through the old 60 s default); on timeout/cancel the command is killed
  and the partial output is kept, with the tool reporting its own `ToolStatus` (`ok`/`error`/`timeout`/`cancelled`)
  instead of the runner turning it into a failed call.
- **New default commands** (`DEFAULT_COMMAND_ALLOWLIST`): search/text (`rg`/`fd`/`jq`/`yq`/`diff`/`sort`/…) and
  build/check toolchains (`gofmt`/`golangci-lint`/clang/gcc/ninja/just/ruff/…). Their host-side execution flags are
  rejected per program in `command_guard::check_tool_specific_args` (`fd -x/-X`, `rg --pre/--hostname-bin/-z`,
  `sort --compress-program`). `sed`/`awk`/`xargs` stay out of the defaults: they are code-execution channels that
  cannot be reliably caught by argument inspection. `git` config escapes (`-c core.sshCommand/hooksPath/…`,
  `git config` writing them) and non-dry-run `git clean` are hard-rejected too — the old alias-only check was not
  enough once AUTO could silently approve commands.
- `liveToolCalls` / `pendingApproval` describe "this turn's generation" only and are deliberately **kept**
  after `stream-end`, so every session change (create / switch / delete / current session disappeared) must
  reset them together — `chatStore.emptyGenerationState()` is the single place doing that. Forgetting it is
  what made finished tool cards reappear inside a brand-new empty session. Session-scoped leftovers
  (`plan`, `snapshot`) are reset there too and re-read per session.
- `plan-updated` is **session-scoped** (no `stream_id`): the task plan is written by `update_plan`, injected
  into the system prompt of every tool-enabled turn (`agent/plan.rs::prompt_section`), and mirrored into the
  UI panel. It is model-authored structured data (titles + status), so it carries no untrusted payload.
  Users may edit the same plan (`update_plan_items` → same table/validation/broadcast); a stopped generation
  gets its `doing` items flipped to `blocked` by `stop_generation` because the model never gets a chance to.
- **Task-mode tool visibility**: Plan is `ToolMode::ReadOnly` (`Permission::Read | WriteSession` — session-only
  writes (`update_plan` / `save_note` / `forget_note`) are exactly what Plan needs, while `save_memory` stays
  `WriteApp`), plus `ToolServices.plan_network` which makes `Network` tools visible in read-only mode (still
  approved per call; subagents always get `false`). **Work is `ToolMode::Full` regardless of `tools.mode`**
  (see `effective_tool_limits` in `commands/chat.rs`): a task session is an explicitly created execution
  context and coding needs `run_command` visible; per-call approval and the command hard deny-list are the
  actual gates, not visibility. Plan and Work share the same step budget (`tools.max_steps.max(20)`); only
  casual chat stays capped at 3 — Plan investigations routinely need more than a handful of rounds before
  `update_plan`.
- **Session-scoped approvals** (`session_grants` table): `AllowSession` is persisted by the runner (via
  `ToolServices.chat_store`) and merged into `auto_approve` in `build_tool_runtime`, so the "本会话允许"
  button really lasts across turns; the task status bar lists/revokes grants. Verify the table is in both
  `MIGRATIONS` and `CREATE_SCRIPTS` (see the schema reconciliation rules above).
- **Session AUTO switch** (`sessions.auto_approve_all`, migration 015): when on, task-session tool calls that
  `requires_approval()` are pre-granted through the separate `HarnessRun.auto_approve_all` bool (never a `"*"`
  wildcard in the user-editable `tools.auto_approve` list). It *only* skips the approval prompt: tool
  visibility, `CommandGuard`, the workspace jail, the deny-glob list, snapshots and the trash stay exactly the
  same, and subagents always get `false` (their approver is still `DenyAllApprover`). It is snapshotted at
  `send_message` time, so the status-bar chip is disabled while streaming; opening it asks for confirmation
  (`AutoApproveConfirm`). `set_session_auto_approve` refuses non-task sessions.
- **Read-only subagents** (`spawn_subagents`): each task runs its own `HarnessRun` with `ToolMode::ReadOnly`
  services, a `DenyAllApprover`, and `services.subagent = None` (depth is therefore exactly 1 by construction,
  not by argument checking). Children share the parent's `cancel` flag and their tool events reuse the parent
  `stream_id` with extra `parent_call_id` / `depth` fields, so the UI counts them on the spawn card instead of
  rendering dozens of unrelated cards. Per-generation budget is `DEFAULT_MAX_CHILDREN` (2), each child gets
  `DEFAULT_CHILD_STEPS` (32, same as the main loop's default) tool rounds, and its token usage is estimated back
  into `HarnessOutcome.extra_tokens` → `message-stats` so the hidden cost stays visible. Tasks in one call run
  **in parallel**, and the whole batch uses `tools.subagent_timeout_secs` (600) through
  `Tool::timeout_budget` instead of the 180 s `call_timeout`; a soft deadline cancels in-flight children and
  returns the completed summaries as `ToolStatus::Timeout` rather than letting the outer timeout drop everything.
- **Working memory** (`save_note` / `forget_note` → `tool_notes`, injected by `agent::notes::prompt_section`):
  the only cross-turn content, and deliberately narrow — model-authored summaries only (never raw tool output),
  ≤8 notes / 2 KB each / 16 KB total with oldest-first eviction, always wrapped in `<untrusted>` markers, never
  allowed to change tool visibility, approvals or config. Gated by `tools.working_memory`, with a UI panel and a
  one-click clear.
- **`web_search`** is the only capability that sends the user's question to a third party, hence `enabled: false`
  by default and `tools.search.{provider,endpoint,api_key,max_results}` must be filled in. Results are links +
  snippets only (untrusted); bodies still go through `web_fetch` and its domain allow-list.
- **MCP** (`src-tauri/src/mcp/`) bridges external tool servers: servers exist only in `config.json` (the model
  can never add/modify/start one), `enabled` and `trusted` both default to false, permission is mapped per server
  (read = no approval, write = approval, execute = approval + full mode only), env is limited to explicitly listed
  keys plus `command_guard::sanitized_env()`. Transport is **blocking** stdio on purpose: `tokio::process::Child`
  dies with the runtime that spawned it, and servers are started on the app-startup path.
- **Model selection & auto-routing** (`llm/router.rs` + `llm/capabilities.rs`): the model for a turn is
  resolved **per generation** in `commands/chat.rs::send_message` (session pref → global active provider) and
  handed to the agent as `AgentContext.models`; `ChatAgent` then builds one request-scoped `LlmProxy` per model.
  Consequences to preserve: (a) switching models mid-generation must never affect the in-flight turn — the whole
  point of resolving at send time; (b) an `inherit` plan (nothing selected) must resolve to exactly the active
  provider the shared backend was built from, so the float window and every no-selection path behave as before
  the feature; `ctx.models = None` (tests, internal callers) still falls back to that shared backend; (c)
  `resolve()` never returns an error — a dangling provider/model degrades to the active provider with a log,
  because model choice must never turn a message into "发送失败". Session prefs live in `sessions.model_pref`
  (single JSON column, migration 011);
  the global main/sub pool lives in `AppConfig.models`. `Auto` mode is **task-sessions only** (Plan → sub model,
  Work → main model; subagents always rotate over `subs`), and `enable_thinking` is only ever sent to models
  judged capable (`llm/capabilities.rs` heuristic + `LlmProvider.thinking_models` explicit override) — strict
  OpenAI-compatible endpoints reject unknown fields with 400 rather than ignoring them.
- File-changing tools (`write_file` overwrite, `edit_file`, `delete_path`, `move_path`, `copy_path`) snapshot
  the affected bytes into `{app_data_dir}/snapshots/{stream_id}/` **before** mutating, indexed in
  `workspace_snapshots`; `get_snapshot` / `restore_snapshot` drive the UI rollback banner. Snapshots are
  file-only (<=4 MB each, <=64 MB per stream) and skipped-with-a-note beyond that — never silently.

### Frontend (src/)

- **Routing**: Zustand state (`currentPage`), no React Router. Pages: `"chat"` | `"settings"` | `"persona"` | `"onboarding"`
- **State**: Single Zustand store (`stores/chatStore.ts`) for sessions, messages, streaming, page nav
- **IPC**: All backend calls use `invoke()` (Tauri commands); streaming uses `listen()` on events
- **Theming**: CSS custom properties in `App.css`, dark (default, Tokyo Night) and light themes toggled via `data-theme` attribute
- **Markdown**: `react-markdown` + `rehype-highlight` + `remark-gfm`; links are opened in the system
  browser via `@tauri-apps/plugin-opener` (never in-app navigation)

### Backend (src-tauri/src/)

**AppState** (`lib.rs`): Global `Mutex`-wrapped state holding `AppConfig`, `AgentDispatcher`, `ChatStore`,
`MemoryStore`, `app_data_dir`, and `cancel_flags` (keyed by `stream_id`, consumed by `stop_generation`).

Layer breakdown:
- **agent/**: `Agent` trait → `AgentDispatcher` → `ChatAgent`. ChatAgent assembles prompts from: persona system prompt + current time + persisted summary + retrieved memories + recent conversation (see context window below) + user input
- **llm/**: `LlmProxy` wraps `OpenAiClient` (OpenAI-compatible API). Supports non-streaming chat, SSE streaming, embeddings, model listing. Uses `reqwest` + `rustls-tls`
- **persona/**: `PersonaEngine` loads YAML personas (built-in via `include_str!` + user files from disk). Variable interpolation (`{user_nickname}`) in system prompts
- **memory/** + **store/memory_store.rs**: LLM-based fact extraction with dedup. Cosine similarity search in Rust over normalized f32 BLOBs. Scoring: `similarity * 0.7 + importance * 0.3`. Vectors whose dimension differs from the current embedding model are skipped, not silently truncated.
- **store/**: Single SQLite database (`data.db`, WAL mode). Migrations in `store/migrations/`, each applied inside a transaction together with its `schema_version` row (never re-run partially). `ChatStore` for sessions/messages/stats, `MemoryStore` for memories, `tool_invocations` for UI-only tool traces, `session_plans` for the task plan (one JSON row per session) and `workspace_snapshots` for rollback
- **Never trust `schema_version` alone**: `init_db` also runs `ensure_schema`, an idempotent reconciliation (`CREATE ... IF NOT EXISTS` scripts + a declarative `REQUIRED_COLUMNS` list probed via `PRAGMA table_info`) on **every** start. A `data.db` written by another build of this app can carry a ledger far ahead of this repo's migrations (a real incident: ledger at 16 while this repo shipped 1–6 → `sessions.context_summary` was never created → app started fine and only failed at `no such column: context_summary` on the first message). New migrations must therefore add their table to `CREATE_SCRIPTS` or their column to `REQUIRED_COLUMNS`; `fresh_and_repaired_schemas_match` fails loudly if you forget
- **commands/**: 60 Tauri IPC commands registered in `lib.rs` — chat, settings, persona, memory, backup, stats, window management, tools (tool list / approvals / session grants / session AUTO / workspace roots)
- **agent/harness/**: tool runtime — `Tool` trait + `ToolRegistry` (mode-gated visibility), SSE `tool_calls` accumulation, the multi-step loop (`runner.rs`), the workspace path jail (`jail.rs`), the sensitive-command guard (`command_guard.rs`) and the approval channel (`approve.rs`). See ARCHITECTURE.md §3.3.

### Context Window

`commands/chat.rs` owns context truncation:
- messages older than the last `CONTEXT_KEEP_RECENT` (10) are folded into `sessions.context_summary`
  by an incremental LLM summary once the session exceeds `CONTEXT_SUMMARIZE_THRESHOLD` (20) messages;
- if summarization fails, the raw window is temporarily widened instead of dropping history.

### Data Storage

All data lives at `%APPDATA%/com.konata-mirror.main/` (Linux: `~/.local/share/com.konata-mirror.main/`):
- `config.json` — app configuration (written atomically: temp file + fsync + rename). If it is ever
  unparseable it is renamed to `config.json.corrupt.<ts>.bak` and defaults are restored instead of crashing.
- `data.db` — SQLite (sessions, messages, memories, usage_stats)
- `personas/` — user-created YAML persona files
- `workspace/` — the tool sandbox's default workspace root (auto-created on first start)

**Identifier history (do not re-merge)**: this app previously shared the bundle id
`com.konata-mirror.app` with a second, parallel development line of the same project (its checkout lives
on another disk and carries scenes / affinity / reminders features). Because both builds wrote the *same*
`data.db` while shipping **conflicting migration numbers** (`005_perf` here vs `005_attachments` there),
that line's ledger reached 16 and silently suppressed this repo's migrations. The id was split to
`com.konata-mirror.main`; `appdata.rs` performs a one-time **copy-only** migration of `data.db` /
`config.json` / `personas/` / `workspace/` from the legacy directory on first start (marker file
`.legacy-data-migrated`, legacy dir left untouched). Never reuse the old identifier for this line.

### Key Patterns

- Backend errors use `anyhow::Result`; Tauri commands return `Result<T, String>` via `.map_err(|e| e.to_string())`
- `LlmConfig::active_provider()` never panics; `LlmConfig::remove_provider` refuses to empty the list, and
  `AppConfig::validate()` rejects self-destructive configs before they are written
- Any value that becomes part of a file path (persona id) must go through
  `commands::persona::write_persona_file` / `validate_persona_id` — never `join` a raw id
- Memory retrieval query is enhanced with the last 3 conversation messages for context
- Auxiliary context reads must degrade, never abort the turn: `commands/chat.rs::read_summary` turns a failed summary read into "no summary this round" (logged) instead of propagating — a schema/query hiccup must never surface as "发送失败"
- LLM transport failures are retried and must stay diagnosable: `OpenAiClient::post_json_with_retry` retries `.send()` failures 3× with backoff (safe — no response bytes were received) and every error is wrapped as `LLM 请求失败 · <model>（<provider>）：…` with the full reqwest `source()` chain (`describe_reqwest_error`), because reqwest's own `Display` hides the real cause behind `error sending request for url`. Never retry after the response stream has started.
- Tool rounds are billed **per assistant message**, not per call: multiple read-only calls in one reply run in parallel and consume a single round, so `read_file` accepts `paths` for bulk reads and the prompts tell the model to batch reads. `tools.max_steps` is user-configurable (1..=128, default 32; Plan/Work share `max(20)`, casual chat stays 3). When the cap is hit the runner forces a tool-less closing turn and `hit_step_limit` flows through `AgentResponse` → `message-stats` so the task bar shows "步数用尽中断" and offers 「继续任务」 — never silently truncate a long task.
- Tools (`agent/harness/`) are **main-window only**: `send_message` receives the Tauri-injected `WebviewWindow` and passes `tools: None` for the `float` window (that window is chat-only — the gate is the window label, never `persist`, because the pet's input box sends persisted messages too). Tool events are emitted with `emit_to("main")`.
- Tool results **never cross turns**: they live in the in-memory message list of one generation, are never written to `messages`, and never reach summary/memory extraction. `tool_invocations` stores previews for UI replay only.
- Every path that becomes a filesystem path must go through `harness::jail::WorkspaceSet` (`resolve` / `resolve_writable`) — never `join` a model-provided path. Command execution goes through `harness::command_guard::CommandGuard`; its hard deny-list always beats the user's allow-list.
- Auto-title generation: LLM generates a short session title after the first message (skipped for `persist: false`)
