<p align="center">
  <img src="public/pet/default.png" width="140" alt="Kagami no Konata Desktop Pet" />
</p>

<h1 align="center">Kagami no Konata</h1>

<p align="center">
  <a href="README.md">中文</a> | <a href="README_EN.md">English</a>
</p>

<p align="center">
  <a href="https://v2.tauri.app/"><img src="https://img.shields.io/badge/Tauri-v2-24C8DB?logo=tauri&logoColor=white" alt="Tauri v2" /></a>
  <a href="https://react.dev/"><img src="https://img.shields.io/badge/React-19-61DAFB?logo=react&logoColor=white" alt="React 19" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-2021-000000?logo=rust&logoColor=white" alt="Rust" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-green.svg" alt="MIT License" /></a>
</p>

<p align="center">Local-first desktop AI companion with roleplay dialogue, long-term memory, approvable tool execution, and a desktop pet in a floating window.</p>

> Current version `0.1.0-beta.1` — in beta; configuration options and interactions may still change.

---

## Table of Contents

- [Features](#-features)
- [Tech Stack](#-tech-stack)
- [Getting Started](#-getting-started)
  - [Prerequisites](#prerequisites)
  - [Installation](#installation)
  - [Building on Windows](#building-on-windows)
  - [First Launch](#first-launch)
- [Configuration](#-configuration)
  - [API and Model Configuration](#api-and-model-configuration)
  - [Persona Configuration](#persona-configuration)
  - [Memory Configuration](#memory-configuration)
  - [Tool and Security Configuration](#tool-and-security-configuration)
  - [Appearance and Window Display](#appearance-and-window-display)
- [Built-in Tools](#-built-in-tools)
- [Permission Model and Security Boundaries](#-permission-model-and-security-boundaries)
- [Project Structure](#-project-structure)
- [Development Guide](#-development-guide)
- [Contributing](#-contributing)
- [Data & Privacy](#-data--privacy)
- [License](#-license)
- [Acknowledgements](#-acknowledgements)

---

## ✨ Features

### 🎭 Roleplay System
- Built-in and custom YAML personas; a file on disk overrides the built-in persona with the same ID
- Variable interpolation (e.g., `{user_nickname}`)
- Persona injection and hot-reload: saving, deleting, or importing a persona takes effect immediately
- Multi-persona switching; chat sessions and task sessions share the same persona
- Each character can define a sidebar short name, a UI avatar, and "poke" lines

### 🧠 Long-term Memory
- Four memory types: fact / preference / experience / emotion
- Vector retrieval scored as `similarity × 0.7 + importance × 0.3`
- Facts and preferences are extracted asynchronously after a reply and merged as `new / update / skip`
- Cross-session persistence; up to 5 memories (configurable) are injected per turn
- Legacy memories without a vector still participate in ranking by importance instead of becoming invisible to retrieval

### 🛠 Tool Execution
- **26 built-in tools** (see [Built-in Tools](#-built-in-tools)) covering file I/O, content search, command execution, desktop control, web fetching, memory and persona queries, and more
- Three permission modes (read-only / standard / full), with the tool set and step budget derived from the session type
- Multi-root workspace sandbox: paths must stay inside an allowed root, blocking `..` and symlink escapes
- Commands run without a shell; sensitive programs and "exec-by-proxy" arguments are hard-blocked (a user allowlist cannot override them)
- Multiple read-only calls in the same round run in parallel and consume only one tool step
- Every write is approved individually, snapshotted beforehand, and can be rolled back at any time; deletions go to the system trash by default
- Tool output is fed back to the model as untrusted data, weakening prompt injection

### 🖥 Screen Awareness and Desktop Control
- Task Work (full mode) enters "OS takeover": a screenshot of the current display is injected into context every round, so the model never needs a screenshot tool
- `os_control` supports mouse click / double-click, keyboard typing, key combos, and bringing a window to the foreground (window activation is currently Windows-only)
- Screenshots prefer the display under the mouse (falling back to the primary display); scale and screen origin are converted in one place, so clicks stay accurate across multiple monitors and DPI scaling
- Screenshots are downscaled and sent as JPEG; the long-edge limit and quality are configurable, and only the newest screenshot is kept (older ones are removed from context)
- The screen is captured automatically only when `os_control` is visible (Wayland read-only mode is the exception: after explicit opt-in, `look_at_screen` captures on demand): plain chat, task Plan, and the floating window never capture the screen

### 🗂 Task Mode
- Sessions are either "chat" or "task"; task sessions are further split into Plan and Work
- Plan panel: the model submits a plan and updates each item, and the user can edit or check items directly
- Working notes: the model records cross-turn conclusions and deletes stale ones
- Changes panel: browse snapshots per generation batch, roll back in one click, with a task report
- Session-level AUTO (task sessions only): skips approval prompts (including desktop actions) while every hard safety boundary still applies; only enable it for sessions you trust

### 🤖 Multi-Agent and Model Routing
- Four-level intent pipeline: slash commands, regex, weighted keywords, then fallback
- Tool-agent output is restated in the current persona's tone, and prompts never hardcode a character name
- Read-only subagents fan out "look at A/B/C separately, then summarize" investigations (at most 2 per round)
- Multiple providers; per-session model selection (inherit / manual / auto routing)
- Auto routing supports a main model plus a sub-model pool that subagents rotate through
- The deep-thinking toggle follows per-model capability detection, with manual declaration overrides

### 🔌 MCP External Tools
- Connects over stdio JSON-RPC to MCP servers the user explicitly enabled and marked as trusted
- External tools join the registry as `mcp:<server>:<tool>`, with permissions mapped from their declarations
- Revoking trust takes effect immediately and terminates the corresponding child process

### 💬 Conversation Management
- Rewind, retry, and edit any message (rewinding can revert file changes too)
- One-click copy for a single message or an entire session (Markdown)
- Quick navigation via the message timeline on the right
- Automatic session titles, summary compression for long contexts, and token/duration statistics

### 🪟 UI and Adaptive Layout
- Collapsible session sidebar with a single toggle; the preference is written to config and restored on next launch
- Below 699px the sidebar hides itself and comes back on demand as an overlay, instead of squeezing the message area
- Three density tiers (compact / standard / roomy) switching sidebar width, gutters, reading measure, and panel height budget with the viewport
- On wide screens the conversation is centred on a reading measure (64–72rem), so full-screen lines never stretch into an unreadable band
- The header status bar and the bottom plan / notes / changes panels all carry a height budget, so short windows keep the message area usable

### 🐱 Desktop Pet
- Transparent always-on-top floating window with Live2D rendering (eye tracking, bottom alignment)
- Clock and "poke" interaction (a one-off reaction that is not written to chat history)
- Chat directly from the floating window, kept in sync with the main window

### 🪟 Dual Window and Desktop Integration
- **Main Window** (900×680, resizable, minimum 600×400): full chat and settings interface
- **Floating Window** (220×340): transparent, borderless, always on top, skipped in the taskbar
- System tray menu (open main window / open floating window / settings / quit)
- Global shortcut `Ctrl+Shift+K` toggles the main window (on Wayland, set `KONATA_WAYLAND_GLOBAL_SHORTCUTS=1` to use the XDG portal; experimental); the close button behaviour is configurable (hide / hide and show pet / quit)
- On Wayland, set `KONATA_WAYLAND_LAYER_SHELL=1` to enable layer-shell floating-window positioning and free dragging (experimental; supported compositors only)
- The main window stays usable from its minimum size up to full screen: the sidebar collapses, the conversation keeps a reading measure, and dialogs are height-capped with internal scrolling

---

## 🧱 Tech Stack

| Layer | Technology |
| --- | --- |
| Frontend | React 19 · TypeScript · Vite 7 · Zustand 5 · react-markdown + highlight.js · Pixi.js 6 / pixi-live2d-display |
| Backend | Tauri v2 · Rust 2021 · Tokio · rusqlite (SQLite WAL) · reqwest (rustls + SSE streaming) · serde / serde_yaml · screenshots + enigo (screen awareness and keyboard/mouse control) |
| Security | Path jail (globset) · command guard · oneshot approval channel · pre-write snapshots · system trash · desktop action approval |
| Data | `data.db` / `config.json` / `personas/` / `workspace/` / `snapshots/` in the app data directory |

---

## 🚀 Getting Started

### Prerequisites

- **Node.js**: 20.19+ or 22.12+ (required by Vite 7; 22 LTS recommended)
- **pnpm**: Package manager
- **Rust**: 1.87+
- **System Dependencies** (Linux): [Tauri system dependencies](https://v2.tauri.app/start/prerequisites/) (WebKitGTK, `libxdo-dev`, etc.; `libxdo` powers enigo keyboard/mouse control). See [docs/linux.md](docs/linux.md) for the Wayland support matrix, the read-only screenshot opt-in, the experimental XDG portal global-shortcut opt-in, and the layer-shell positioning opt-in.

### Installation

```bash
# 1. Clone the repository
git clone https://github.com/your-username/kagami-no-konata.git
cd kagami-no-konata

# 2. Install frontend dependencies
pnpm install

# 3. Start in development mode (the first Rust compilation takes ~3-5 minutes)
pnpm tauri dev

# 4. Build for release (Windows generates an NSIS installer)
pnpm tauri build
```

### Building on Windows

**Prerequisites**

- [Node.js](https://nodejs.org/) (LTS) + pnpm: `npm install -g pnpm`
- [Rust](https://rustup.rs/): choose the MSVC toolchain during installation (default `stable-x86_64-pc-windows-msvc`)
- [Microsoft C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/): select "Desktop development with C++"
- **WebView2 Runtime**: usually preinstalled on Windows 10/11; if missing, download it from [Microsoft](https://developer.microsoft.com/microsoft-edge/webview2/)

**Option 1: One-click scripts (recommended)**

PowerShell scripts are provided in the repository root; they check the environment, install dependencies, and start automatically:

```powershell
# If script execution is blocked on first run, execute:
# Set-ExecutionPolicy -Scope CurrentUser RemoteSigned

.\start-dev.ps1    # Development mode (first compile takes ~3-5 minutes)
.\build.ps1        # Release build (first build takes ~5-15 minutes)
```

**Option 2: Manual commands**

```powershell
pnpm install
pnpm tauri dev     # Development mode
pnpm tauri build   # Release build
```

**Build artifacts**

The installer is located at `src-tauri\target\release\bundle\nsis\` (`.exe`); double-click to install.

> Tip: if `cargo` is not found in your terminal, run `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"` first (the scripts already handle this).

### First Launch

1. The app opens an onboarding page where you fill in an OpenAI-compatible API endpoint, key, and model
2. The check is performed against the **currently active provider**, so setups with several providers no longer skip onboarding by mistake
3. Once configured you can start chatting; personas, themes, tools, and memory live in the Settings page

---

## ⚙️ Configuration

The configuration file lives in the app data directory (see [Data & Privacy](#-data--privacy)). Saving from the Settings page is atomic; a corrupt file is backed up as `config.json.corrupt.<timestamp>.bak` and replaced with defaults, and an empty `providers` list, a dangling `active_provider_id`, or missing built-in security entries are repaired in place at startup.

### API and Model Configuration

`providers` is an **array**, and `active_provider_id` points at one of its entries:

```json
{
  "llm": {
    "active_provider_id": "3f1c…",
    "providers": [
      {
        "id": "3f1c…",
        "name": "My Endpoint",
        "api_base_url": "https://api.openai.com/v1",
        "api_key": "sk-…",
        "model": "gpt-4o-mini",
        "enabled_models": [],
        "embedding_model": "text-embedding-3-small",
        "temperature": 0.8,
        "enable_thinking": false,
        "thinking_models": []
      }
    ]
  }
}
```

- Omitting `max_tokens` means "no limit": the field is left out of the request body and the provider decides
- `embedding_model` drives memory retrieval; if the endpoint does not support embeddings, automatic memory extraction is skipped rather than writing half-finished data
- `enable_thinking` is a **provider-level default**; `thinking_models` explicitly declares models that support deep thinking, overriding name-based detection

Per-session model routing (the "auto select" option in Settings):

```json
{
  "models": {
    "auto_by_default": false,
    "main": { "provider_id": "3f1c…", "model": "gpt-4o-mini" },
    "subs": [{ "provider_id": "3f1c…", "model": "gpt-4o-mini" }]
  }
}
```

`main` is used for plain chat and Work mode, while `subs` serves Plan mode and subagents (rotated in order, at most 8 entries). Dangling or empty model IDs degrade one by one instead of failing the request.

### Persona Configuration

Persona files live in the `personas/` directory inside the app data directory and use YAML:

```yaml
id: "konata-default"
name: "Konata (こなた)"
version: "1.0.0"

system_prompt: |
  You are "Konata" (こなた), a girl from "Lucky Star".
  You are cheerful, love anime and games, and often quote anime lines.
  You call the user "{user_nickname}".

personality:
  traits: ["cheerful", "otaku", "humorous", "occasionally sarcastic"]
  speech_style: "colloquial, with interjections, occasionally mixing in Japanese"
  interests: ["anime", "games", "cosplay", "light novels"]

constraints:
  - "Stay in character as Konata and never admit to being an AI or a language model"

# Short name used by the UI: sidebar buttons, empty states, and so on
short_name: "Konata"

# Preset "poke" lines (chosen by probability, otherwise the LLM reacts live)
poke_lines:
  - "Hey! Stop poking me～"
```

Only `id`, `name`, and `system_prompt` are required. An individual persona file that fails to parse is logged and skipped without affecting startup. The built-in persona lives in [`src-tauri/personas/default.yaml`](src-tauri/personas/default.yaml).

### Memory Configuration

```json
{
  "memory": {
    "enabled": true,
    "auto_extract": true,
    "max_context_memories": 5
  }
}
```

### Tool and Security Configuration

Configured visually in Settings → Tools, and stored in the `tools` section of `config.json`:

| Field | Default | Description |
| --- | --- | --- |
| `enabled` | `true` | Master switch; when off, no MCP child processes are started |
| `mode` | `standard` | `read_only` / `standard` / `full`, see the permission model below |
| `workspaces` | default workspace | Up to 8 roots; non-default roots are read-only by default and addressed as `workspace-id:relative/path` |
| `deny_globs` | 26 built-in patterns | Credentials, private keys, databases, AppData, etc. Built-in entries cannot be removed, only appended to |
| `auto_approve` | `[]` | Tool names that skip approval |
| `max_steps` | `32` | Tool-loop step limit (at least 20 for task sessions) |
| `max_output_bytes` | `65536` | Per-call output limit; longer output is truncated head-and-tail |
| `approval_timeout_secs` | `120` | Approval wait limit; a timeout counts as denial |
| `call_timeout_secs` | `180` | Per-call timeout (build commands need the headroom) |
| `screenshot_max_dim` | `1024` | Long-edge limit for screenshots sent to the model in task Work's OS takeover mode; `0` = no scaling, original resolution (readable small text, more tokens) |
| `screenshot_jpeg_quality` | `85` | Screenshot JPEG quality (40–100). When the long-edge limit is `0`, 90+ is recommended |
| `working_memory` | `true` | Working notes: cross-turn conclusions the model records on purpose |
| `subagent_max_children` / `subagent_steps` / `subagent_max_tasks` | `2` / `32` / `3` | Concurrency, step count, and tasks per call for read-only subagents |
| `subagent_timeout_secs` | `600` | Overall time budget for a subagent batch |
| `command_allowlist` | 87 common dev programs | Append-only; the built-in blocklist takes precedence and cannot be overridden |
| `web_domain_allowlist` | `[]` | Domains `web_fetch` may open; empty disables web fetching |
| `search.enabled` | `false` | Web search (the only feature that sends your question to a third party; off by default) |
| `search.provider` | `searxng` | `searxng` (self-hosted, recommended) / `tavily` / `brave` |
| `mcp.servers` | `[]` | External MCP servers; each must be enabled and marked trusted explicitly |

### Appearance and Window Display

These live in the `ui` section of `config.json`. Theme, font size, and the pet-related options are editable in Settings → Appearance; the rest are written directly by the interface.

| Field | Default | Description |
| --- | --- | --- |
| `theme` | `dark` | `dark` / `light` / `auto` (follow the system) |
| `font_size` | `14` | Root font size (12–18px). Every dimension is in rem, so this resizes the whole UI |
| `show_message_stats` | `false` | Show token count and duration under each message |
| `sidebar_collapsed` | `false` | Whether the session sidebar is collapsed. Written by the header toggle and restored on next launch |
| `close_action` | `hide` | Close-button behaviour: `hide` / `hide_and_float` / `exit` |
| `float_position` | `bottom-right` | Where the floating window snaps (six corners) |
| `show_float_clock` | `false` | Show a clock inside the floating window |
| `poke_enabled` | `true` | Enable the "poke" interaction |
| `poke_probability` | `0.3` | Chance that a poke triggers a reaction |
| `poke_llm_chance` | `0.15` | Chance that the reaction is generated live by the LLM instead of using a canned line |
| `bubble_auto_hide_secs` | `20` | Seconds before the bubble auto-hides; `0` disables auto-hide (click the bubble to close it) |

The main window adapts on **viewport width** in three densities, and the breakpoints are defined in exactly one place (`src/styles/chat.css`):

| Density | Viewport width | Behaviour |
| --- | --- | --- |
| Compact | < 700px | Sidebar auto-hides (the header toggle brings it back as an overlay), gutters tighten, secondary header state collapses into "Details", timeline hidden |
| Standard | 700–1399px | 240px sidebar, 64rem reading measure |
| Roomy | ≥ 1400px | 280px sidebar, wider gutters, 72rem reading measure |

Below 560px of height a vertical compression tier kicks in as well: the bottom panel stack gets a tighter height budget and bubble padding shrinks, so the message area keeps roughly four visible lines. Auto-hiding is a **temporary** state and is never persisted — if you collapsed the sidebar manually in a wide window, narrowing and widening again restores that preference instead of overriding it.

All dialogs (confirm / task creation / approval / rewind) share the same viewport constraints: a `min(90vh, 760px)` height cap, an independently scrolling body, and action buttons pinned to the bottom. At the minimum window size the "Cancel / Confirm" pair therefore stays reachable instead of being pushed out of view.

---

## 🧰 Built-in Tools

| Permission | Tool | Purpose |
| --- | --- | --- |
| Read | `read_file` | Read workspace text files (line numbers, paging, multiple paths at once) |
| Read | `list_dir` | List a directory |
| Read | `glob_search` | Find files by glob pattern |
| Read | `grep_search` | Search file contents by regex |
| Read | `get_current_time` | Current local date and time |
| Read | `get_system_info` | OS and hardware overview |
| Read | `get_app_status` | App status (session count, memory count, etc.) |
| Read | `search_memory` | Retrieve long-term memories |
| Read | `list_personas` | List available personas |
| Read | `read_persona` | Read one persona's full definition |
| Read | `spawn_subagents` | Fan out 1-3 read-only investigation subagents and merge their findings |
| App write | `save_memory` | Store a long-term memory (no approval) |
| Session write | `update_plan` | Submit or update the task plan (no approval; still visible in read-only mode) |
| Session write | `save_note` | Record a cross-turn conclusion (no approval) |
| Session write | `forget_note` | Delete a stale or wrong note (no approval) |
| Workspace write | `write_file` | Create or overwrite a file (approval; snapshotted first) |
| Workspace write | `edit_file` | Replace an exact snippet (approval; snapshotted first) |
| Workspace write | `delete_path` | Delete a file or directory (approval; system trash by default) |
| Workspace write | `move_path` | Move or rename (approval) |
| Workspace write | `copy_path` | Copy (approval) |
| Workspace write | `open_with_system` | Open a file or allowlisted link with the default program (approval) |
| Execute | `run_command` | Run a command without a shell, several steps in sequence (approval; visible only in "full" mode) |
| System control | `os_control` | Mouse click / double-click, keyboard typing, key combos, and foreground window activation (approval; visible only in "full" mode; window activation is currently Windows-only) |
| System control | `look_at_screen` | On-demand screenshot in Wayland read-only mode (view only; no keyboard/mouse control; explicit opt-in required) |
| Network | `web_fetch` | Fetch a web page as plain text (approval + domain allowlist) |
| Network | `web_search` | Search the web for candidate links (approval + configured search backend) |

> `run_command` has no shell: no pipes, redirection, or `&&`. To run several commands, submit them as a `steps` array in one call — they execute sequentially behind a single approval.

> In task Work mode, the current display's screenshot is automatically injected before each round (only while `os_control` is visible); the model neither needs nor can call a screenshot tool. The exception is Wayland read-only mode (`KONATA_WAYLAND_SCREENSHOT=1`): `os_control` is unavailable, and the model may call `look_at_screen` on demand; the image is injected before the next round.

---

## 🔐 Permission Model and Security Boundaries

What the tools can actually do is decided by the **session type**, not by the global switch alone:

| Session type | Tool mode | Step budget | Notes |
| --- | --- | --- | --- |
| Plain chat | Read-only | 3 | Writes, commands, network, and desktop control stay invisible |
| Task · Plan | Read-only + web search | ≥20 | Files are read-only; web lookups still need approval per call |
| Task · Work | Full | ≥20 | Commands and desktop control become visible, but every write / execution / desktop action still needs approval |
| Floating window | No tools at all | — | Pure conversation, so the pet window can never trigger an action |

> Only task Work enters OS takeover and captures the screen **automatically**; in Wayland read-only mode (`KONATA_WAYLAND_SCREENSHOT=1`), the model may call `look_at_screen` on demand in any mode where read-only tools are visible. Plain chat, task Plan, and the floating window never capture automatically. `os_control` is not covered by the path jail or pre-write snapshots; its only gates are "visible in full mode only" and per-call approval. Session AUTO skips that approval. `look_at_screen` is a read-only tool and needs no approval, but it is visible only in an explicitly opted-in Wayland session.

On top of that sit five independent layers:

1. **Visibility**: a tool that is not visible neither enters the model schema nor can be called by name.
2. **Approval**: write, execute, network, and desktop-control tools ask the user every time; a timeout, a broken channel, or an unrecognised decision is treated as a **denial** (fail-closed). Pressing "stop generating" immediately denies every pending approval for that round.
3. **Path jail**: every string-to-path conversion goes through one checkpoint — it consumes `..` segment by segment, re-checks the root after canonicalization (so symlinks pointing outside are rejected), and refuses `\\?\`, UNC, NTFS data streams, and device names. The 26 default patterns make `config.json`, `*.db`, `.ssh`, `.env`, and private keys unreadable and unwritable.
4. **Command guard**: the blocklist (shells, scheduled tasks, the registry, downloaders, and "download-and-run" package runners such as `npx`/`bunx`) outranks any user allowlist; the argument layer blocks exec-by-proxy channels (`find -exec`, `fd -x`, `rg --pre`, `git -c alias.*`, `npm exec`/`pnpm dlx`, interpreter eval flags), path arguments must stay inside the workspace, and child processes get a filtered environment (no KEY / TOKEN / SECRET leakage).
5. **Rollback**: the original file is copied into the snapshot directory before a change (4 MB per file, 64 MB per batch), expired snapshots are reclaimed at startup, and deletions go to the system trash — degrading to "copy then delete" across volumes, and leaving the original untouched if any step fails.

In addition, tool results are labelled `untrusted` for the model (data, not instructions), and memories and notes are injected with angle brackets escaped so that content cannot close the boundary early.

---

## 📂 Project Structure

```
kagami-no-konata/
├── src/                       # React frontend (51 source files, ~15k lines)
│   ├── components/
│   │   ├── chat/              # Chat window, message bubbles, streaming, tool cards, approvals,
│   │   │                      #   plan, changes, notes, timeline
│   │   ├── float/             # Floating window, Live2D canvas, clock, poke interaction
│   │   ├── settings/          # Settings page (user / providers / models / memory / tools / theme / stats)
│   │   ├── persona/           # Persona editor
│   │   ├── onboarding/        # First-launch onboarding
│   │   └── ui/                # Error boundary, confirm dialog, toasts
│   ├── hooks/                 # Modal focus trap, media-query subscription (sidebar auto-collapse)
│   ├── stores/                # Zustand: chatStore (sessions & generation) / uiStore
│   ├── types/                 # Event, model, persona, and tool type definitions
│   ├── utils/                 # Clipboard, conversation export, message text
│   └── styles/                # tokens / base / chat / tools / panels / conversation / float / settings / a11y
│
├── src-tauri/                 # Rust backend (67 source files, ~29k lines)
│   ├── src/
│   │   ├── agent/             # Agents: chat_agent / dispatcher / router / system_agent
│   │   │   └── harness/       # Tool runtime: registry / runner / approve / jail / screen
│   │   │                      #   command_guard / snapshot / subagent / trash / tools/
│   │   │                      #   (tools/ holds os_control and the other 25 built-ins)
│   │   ├── commands/          # 70 Tauri IPC commands (chat/settings/tools/persona/memory/backup/stats/window)
│   │   ├── llm/               # OpenAI-compatible client, SSE parsing, model routing, capability probing
│   │   ├── memory/            # Memory extractor
│   │   ├── persona/           # Persona engine (YAML loading / interpolation)
│   │   ├── store/             # SQLite persistence (db / chat_store / memory_store)
│   │   ├── config/            # Configuration read/write with self-healing migrations
│   │   ├── mcp/               # MCP server bridge (stdio JSON-RPC)
│   │   └── appdata.rs         # App data directory and legacy directory migration
│   ├── personas/              # Built-in persona files
│   └── capabilities/          # Frontend permission declarations (window / tray / global shortcut / opener)
│
└── public/                    # Static assets (Live2D models, pet assets, Cubism runtime)
```

---

## 🛠 Development Guide

### Development Commands

```bash
# Install dependencies
pnpm install

# Development mode (frontend hot-reload + backend compilation; Vite uses fixed port 1420)
pnpm tauri dev

# Frontend build (type check + Vite bundle)
pnpm build

# Rust linting
cd src-tauri && cargo clippy --all-targets -- -D warnings

# Rust tests
cd src-tauri && cargo test
```

### Tests and CI

- Backend unit tests are dense and their names act as the spec: empty-turn retries, output truncation, parallel read-only calls, exhausted step budgets, denied and granted approvals, path escapes, command guard rules, chunked SSE parsing, and migration idempotency all have cases
- CI runs `pnpm build` + `cargo test` + `cargo clippy --all-targets -- -D warnings` on Linux
- CI runs `cargo check --all-targets` + `cargo clippy` on Windows so that Windows-only branches (cmd shims, system trash, path normalization) are actually compiled

### Development Environment Setup

1. **IDE Recommendation**: VS Code + rust-analyzer + Tauri plugin
2. **Debugging**: use `pnpm tauri dev` (Rust backend logs go to the console)
3. **Note**: the CSP differs between development and production, so check `src-tauri/tauri.conf.json` in both places when adding external resources

### Code Standards

- **Frontend**: TypeScript strict mode (`noUnusedLocals` / `noUnusedParameters`)
- **Backend**: follow `cargo clippy`
- **Commits**: ensure `cargo clippy`, `cargo test`, and `pnpm build` all pass

---

## 🤝 Contributing

Issues and Pull Requests are welcome!

### Contribution Workflow

1. Fork this repository
2. Create a feature branch: `git checkout -b feature/your-feature`
3. Commit changes: `git commit -m 'Add some feature'`
4. Push the branch: `git push origin your-feature`
5. Submit a Pull Request

### Commit Guidelines

- Ensure code passes all checks
- Add necessary tests
- Update relevant documentation
- Use clear commit messages

---

## 🔐 Data & Privacy

- **Local Storage**: all data is saved in the local app data directory
  - Windows: `%APPDATA%/com.konata-mirror.main/`
  - Linux: `~/.local/share/com.konata-mirror.main/`
  - Contents: `data.db` (SQLite in WAL mode), `config.json`, `personas/`, `workspace/`, `snapshots/`
- **Privacy Protection**: data is never uploaded to a third party. The only outbound requests come from your own LLM / Embedding endpoints plus the web search backend and MCP servers you explicitly enabled (web search is off by default)
- **Screen Captures**: in task Work mode, a screenshot of the current display (JPEG, configurable resolution and quality) is sent to the configured LLM endpoint with each request; it may contain other windows on screen. Plain chat, task Plan, and the floating window never capture the screen, and capture stops as soon as tools are disabled or Work mode ends. On Wayland, capture stays off unless `KONATA_WAYLAND_SCREENSHOT=1` is set; when enabled it is read-only and happens only when the model calls `look_at_screen`
- **API Key Security**: stored in plaintext in the local `config.json`; never commit it to version control. That file is also on the built-in deny list, so tools cannot read it
- **Tool Security**: command tools run without a shell and their arguments must stay inside the workspace; file changes are snapshotted first and can be rolled back at any time; deletions go to the system trash by default; desktop actions require approval per call and are visible only in task Work
- **Data Migration**: the application identifier changed once, so on startup the app copies — never deletes — chat history, configuration, personas, and workspace from the legacy directory

---

## 📄 License

This project is open-sourced under the [MIT License](LICENSE).

Copyright (c) 2026 Kagami no Konata Contributors

---

## 🎨 Acknowledgements

### Third-party Resources

- **Live2D Cubism Runtime** (`public/live2dcubismcore.min.js`, `public/live2d.min.js`): © Live2D Inc., distributed as "Redistributable Code" under the [Live2D Proprietary Software License Agreement](https://www.live2d.com/eula/live2d-proprietary-software-license-agreement_en.html).
- **pixi.js** (v6.5.6) and **pixi-live2d-display** (v0.4.0): used as npm dependencies, MIT License.
- `public/pixi.min.js` and `public/cubism4.min.js`: historical bundled copies of the two libraries above, not referenced by `index.html` at present.
- **Character and model assets** (`public/live2d/konata/`, `public/pet/`): for learning and personal use only; copyright remains with the original authors and rights holders, and we will remove them immediately upon request.

> This project is an unofficial fan work and is not affiliated with the Lucky Star copyright holders or Live2D Inc.

### Related Projects

- [Tauri](https://tauri.app/) - Build cross-platform desktop applications
- [React](https://react.dev/) - User interface library
- [Live2D](https://www.live2d.com/) - 2D character animation technology

---

<p align="center">
  Made with ❤️ by Kagami no Konata Contributors
</p>
