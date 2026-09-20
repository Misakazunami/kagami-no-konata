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
- [Project Structure](#-project-structure)
- [Development Guide](#-development-guide)
- [Contributing](#-contributing)
- [Data & Privacy](#-data--privacy)
- [License](#-license)
- [Acknowledgements](#-acknowledgements)

---

## ✨ Features

### 🎭 Roleplay System
- Built-in and custom YAML persona configurations
- Variable interpolation (e.g., `{user_nickname}`)
- Persona injection and hot-reload
- Multi-persona switching

### 🧠 Long-term Memory
- Vector retrieval (`similarity × 0.7 + importance × 0.3`)
- Async fact and preference extraction from conversations with deduplication
- Cross-session memory persistence

### 🛠 Tool Execution
- 16 built-in tools (file read/write, content search, command execution, web scraping, etc.)
- Three-tier permission control
- Workspace sandbox isolation
- Hard block on sensitive commands
- Per-approval for write operations with rollback support

### 🤖 Multi-Agent Routing
- Slash commands / regex / keyword four-level intent pipeline
- Tool agent output can be restated in persona's tone

### 💬 Conversation Management
- Rewind, retry, and edit any message
- One-click copy for single message or entire session (Markdown)
- Quick navigation via message timeline on the right

### 🐱 Desktop Pet
- Transparent always-on-top floating window
- Live2D rendering
- Clock and "poke" interaction (one-time reaction, not saved to chat history)

### 🪟 Dual Window Design
- **Main Window** (900×680): Full chat and settings interface
- **Floating Window** (220×340): Lightweight desktop companion

---

## 🧱 Tech Stack

| Layer | Technology |
| --- | --- |
| Frontend | React 19 · TypeScript · Vite · Zustand · Pixi.js / Live2D |
| Backend | Tauri v2 · Rust · Tokio · rusqlite (SQLite WAL) · reqwest (SSE streaming) |
| Data | `data.db` / `config.json` / `personas/` / `workspace/` in app data directory |

---

## 🚀 Getting Started

### Prerequisites

- **Node.js**: LTS version (recommended 18+)
- **pnpm**: Package manager
- **Rust**: 1.87+
- **System Dependencies** (Linux): [Tauri system dependencies](https://v2.tauri.app/start/prerequisites/) (WebKitGTK, etc.)

### Installation

```bash
# 1. Clone the repository
git clone https://github.com/your-username/kagami-no-konata.git
cd kagami-no-konata

# 2. Install frontend dependencies
pnpm install

# 3. Start in development mode (first Rust compilation takes ~3-5 minutes)
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

1. After launching the app, the system will guide you to configure an OpenAI-compatible API endpoint and model
2. Once configured, you can start chatting
3. Adjust persona, theme, and other settings in the Settings page

---

## ⚙️ Configuration

### API Configuration

Configure in the Settings page or `config.json`:

```json
{
  "llm": {
    "active_provider": "openai",
    "providers": {
      "openai": {
        "api_base_url": "https://api.openai.com/v1",
        "api_key": "your-api-key",
        "model": "gpt-4o-mini"
      }
    }
  }
}
```

### Persona Configuration

Persona files are located in the `personas/` directory, supporting YAML format:

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
```

### Tool Configuration

In Settings → Tools, you can configure:
- Tool mode (read-only/full/custom)
- Command whitelist
- Workspace paths
- Subagent configuration

---

## 📂 Project Structure

```
kagami-no-konata/
├── src/                       # React frontend
│   ├── components/            # UI components (chat/float/settings/persona/onboarding)
│   ├── stores/                # Zustand state management
│   ├── types/                 # TypeScript type definitions
│   └── utils/                 # Utility functions
│
├── src-tauri/                 # Rust backend
│   ├── src/
│   │   ├── agent/             # Agent module (chat/routing/tool runtime)
│   │   ├── commands/          # Tauri IPC commands (60+)
│   │   ├── llm/               # LLM proxy layer (OpenAI compatible/model routing)
│   │   ├── memory/            # Memory system (extraction/retrieval)
│   │   ├── persona/           # Persona engine (YAML loading/variable interpolation)
│   │   ├── store/             # Data persistence (SQLite)
│   │   ├── config/            # Configuration management
│   │   └── mcp/               # MCP server bridging
│   └── personas/              # Built-in persona files
│
└── public/                    # Static assets (Live2D models/pet assets)
```

---

## 🛠 Development Guide

### Development Commands

```bash
# Install dependencies
pnpm install

# Development mode (frontend hot-reload + backend compilation)
pnpm tauri dev

# Frontend build (type check + Vite bundle)
pnpm build

# Rust linting
cd src-tauri && cargo clippy

# Rust tests
cd src-tauri && cargo test
```

### Development Environment Setup

1. **IDE Recommendation**: VS Code + rust-analyzer + Tauri plugin
2. **Debugging**: Use `pnpm tauri dev` to start development mode
3. **Logging**: Rust backend logs output to console

### Code Standards

- **Frontend**: Follow ESLint configuration
- **Backend**: Follow `cargo clippy` standards
- **Commits**: Ensure `cargo clippy`, `cargo test`, and `pnpm build` all pass

---

## 🤝 Contributing

Issues and Pull Requests are welcome!

### Contribution Workflow

1. Fork this repository
2. Create a feature branch: `git checkout -b feature/your-feature`
3. Commit changes: `git commit -m 'Add some feature'`
4. Push branch: `git push origin feature/your-feature`
5. Submit a Pull Request

### Commit Guidelines

- Ensure code passes all checks
- Add necessary tests
- Update relevant documentation
- Use clear commit messages

---

## 🔐 Data & Privacy

- **Local Storage**: All data is saved in the local app data directory
  - Windows: `%APPDATA%/com.konata-mirror.main/`
  - Linux: `~/.local/share/com.konata-mirror.main/`
- **Privacy Protection**: Data is not uploaded to any third party; the only external requests come from your configured LLM / Embedding endpoints
- **API Key Security**: Stored in plaintext in local `config.json`; do not commit to version control
- **Tool Security**: Command tools execute without shell; parameters must stay within workspace; file changes are snapshotted first and can be rolled back anytime

---

## 📄 License

This project is open-sourced under the [MIT License](LICENSE).

Copyright (c) 2026 Kagami no Konata Contributors

---

## 🎨 Acknowledgements

### Third-party Resources

- **Live2D Cubism Runtime** (`public/live2dcubismcore.min.js`, `public/live2d.min.js`): © Live2D Inc., distributed as "Redistributable Code" under the [Live2D Proprietary Software License Agreement](https://www.live2d.com/eula/live2d-proprietary-software-license-agreement_en.html).
- **pixi.js** (`public/pixi.min.js`, v7.3.2) and **pixi-live2d-display** (`public/cubism4.min.js`): MIT License.
- **Character and model assets** (`public/live2d/konata/`, `public/pet/`): For learning and personal use only;版权归原作者及权利人所有；if rights holders request removal, we will comply immediately.

> This project is an unofficial fan work and is not affiliated with the Lucky Star copyright holders or Live2D Inc.

### Related Projects

- [Tauri](https://tauri.app/) - Build cross-platform desktop applications
- [React](https://react.dev/) - User interface library
- [Live2D](https://www.live2d.com/) - 2D character animation technology

---

<p align="center">
  Made with ❤️ by Kagami no Konata Contributors
</p>