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
| `message-stats` | `{ session_id, stream_id, token_count, thinking_ms }` |
| `stream-error` | `{ session_id, stream_id, message }` |

- `session_id` isolates conversations; `stream_id` isolates concurrent generations
  (both windows normally share the same "today" session).
- The single source of truth for these shapes is `src/types/events.ts`. Do not redeclare them per component.
- `send_message` takes an optional `stream_id` (auto-generated if omitted) and `persist: false`
  for one-shot interactions (poke reactions) that must not touch the database.
- `isStreaming` must always be reset in a `finally` block — never rely on an event arriving.

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
- **store/**: Single SQLite database (`data.db`, WAL mode). Migrations in `store/migrations/`, each applied inside a transaction together with its `schema_version` row (never re-run partially). `ChatStore` for sessions/messages/stats, `MemoryStore` for memories, `tool_invocations` for UI-only tool traces
- **Never trust `schema_version` alone**: `init_db` also runs `ensure_schema`, an idempotent reconciliation (`CREATE ... IF NOT EXISTS` scripts + a declarative `REQUIRED_COLUMNS` list probed via `PRAGMA table_info`) on **every** start. A `data.db` written by another build of this app can carry a ledger far ahead of this repo's migrations (a real incident: ledger at 16 while this repo shipped 1–6 → `sessions.context_summary` was never created → app started fine and only failed at `no such column: context_summary` on the first message). New migrations must therefore add their table to `CREATE_SCRIPTS` or their column to `REQUIRED_COLUMNS`; `fresh_and_repaired_schemas_match` fails loudly if you forget
- **commands/**: 47 Tauri IPC commands registered in `lib.rs` — chat, settings, persona, memory, backup, stats, window management, tools (tool list / approvals / workspace roots)
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
- Tools (`agent/harness/`) are **main-window only**: `send_message` receives the Tauri-injected `WebviewWindow` and passes `tools: None` for the `float` window (that window is chat-only — the gate is the window label, never `persist`, because the pet's input box sends persisted messages too). Tool events are emitted with `emit_to("main")`.
- Tool results **never cross turns**: they live in the in-memory message list of one generation, are never written to `messages`, and never reach summary/memory extraction. `tool_invocations` stores previews for UI replay only.
- Every path that becomes a filesystem path must go through `harness::jail::WorkspaceSet` (`resolve` / `resolve_writable`) — never `join` a model-provided path. Command execution goes through `harness::command_guard::CommandGuard`; its hard deny-list always beats the user's allow-list.
- Auto-title generation: LLM generates a short session title after the first message (skipped for `persist: false`)
