<p align="center">
  <img src="public/pet/default.png" width="140" alt="Kagami no Konata 桌面宠物" />
</p>

<h1 align="center">Kagami no Konata（镜中此方）</h1>

<p align="center">
  <a href="README.md">中文</a> | <a href="README_EN.md">English</a>
</p>

<p align="center">
  <a href="https://v2.tauri.app/"><img src="https://img.shields.io/badge/Tauri-v2-24C8DB?logo=tauri&logoColor=white" alt="Tauri v2" /></a>
  <a href="https://react.dev/"><img src="https://img.shields.io/badge/React-19-61DAFB?logo=react&logoColor=white" alt="React 19" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-2021-000000?logo=rust&logoColor=white" alt="Rust" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-green.svg" alt="MIT License" /></a>
</p>

<p align="center">本地优先的桌面 AI 伴侣：角色扮演对话、长期记忆、可审批的工具执行，以及一只住在悬浮窗里的桌宠。</p>

---

## 目录

- [功能特性](#-功能特性)
- [技术栈](#-技术栈)
- [快速开始](#-快速开始)
  - [环境要求](#环境要求)
  - [安装步骤](#安装步骤)
  - [Windows 构建](#windows-构建)
  - [首次启动](#首次启动)
- [配置说明](#-配置说明)
- [项目结构](#-项目结构)
- [开发指南](#-开发指南)
- [贡献指南](#-贡献指南)
- [数据与隐私](#-数据与隐私)
- [许可证](#-许可证)
- [致谢](#-致谢)

---

## ✨ 功能特性

### 🎭 角色扮演系统
- 内置与自定义 YAML 人格配置
- 支持变量插值（如 `{user_nickname}`）
- 人设注入与热重载
- 多人格切换

### 🧠 长期记忆
- 向量检索（`相似度 × 0.7 + 重要性 × 0.3`）
- 对话异步提取事实与偏好并去重更新
- 跨会话记忆持久化

### 🛠 工具执行
- 16 个内置工具（文件读写、内容搜索、命令执行、网页抓取等）
- 三档权限控制
- 工作区沙箱隔离
- 敏感命令硬拦截
- 写操作逐次审批并可回滚

### 🤖 多智能体路由
- 斜杠指令 / 正则 / 关键词四级意图流水线
- 工具型智能体的输出可按人设语气转述

### 💬 对话管理
- 任意消息的回退、重试与编辑
- 单条 / 整会话一键复制（Markdown）
- 右侧消息时间轴快速跳转

### 🐱 桌面宠物
- 透明置顶悬浮窗
- Live2D 渲染
- 时钟与「戳一戳」互动（一次性反应，不写入聊天记录）

### 🪟 双窗口设计
- **主窗口**（900×680）：完整聊天与设置界面
- **悬浮窗**（220×340）：轻量桌面伴侣

---

## 🧱 技术栈

| 层 | 技术 |
| --- | --- |
| 前端 | React 19 · TypeScript · Vite · Zustand · Pixi.js / Live2D |
| 后端 | Tauri v2 · Rust · Tokio · rusqlite（SQLite WAL）· reqwest（SSE 流式） |
| 数据 | 应用数据目录下的 `data.db` / `config.json` / `personas/` / `workspace/` |

---

## 🚀 快速开始

### 环境要求

- **Node.js**：LTS 版本（推荐 18+）
- **pnpm**：包管理器
- **Rust**：1.87+
- **系统依赖**（Linux）：[Tauri 系统依赖](https://v2.tauri.app/start/prerequisites/)（WebKitGTK 等）

### 安装步骤

```bash
# 1. 克隆仓库
git clone https://github.com/your-username/kagami-no-konata.git
cd kagami-no-konata

# 2. 安装前端依赖
pnpm install

# 3. 开发模式启动（首次编译 Rust 约 3–5 分钟）
pnpm tauri dev

# 4. 发布构建（Windows 生成 NSIS 安装程序）
pnpm tauri build
```

### Windows 构建

**前置依赖**

- [Node.js](https://nodejs.org/)（LTS）+ pnpm：`npm install -g pnpm`
- [Rust](https://rustup.rs/)：安装时选择 MSVC 工具链（默认 `stable-x86_64-pc-windows-msvc`）
- [Microsoft C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)：安装时勾选「使用 C++ 的桌面开发」
- **WebView2 Runtime**：Windows 10/11 通常已预装；若缺失可从[微软官网](https://developer.microsoft.com/microsoft-edge/webview2/)下载

**方式一：一键脚本（推荐）**

仓库根目录提供 PowerShell 脚本，会自动检查环境、安装依赖并启动：

```powershell
# 若首次运行提示脚本被禁止，先执行：
# Set-ExecutionPolicy -Scope CurrentUser RemoteSigned

.\start-dev.ps1    # 开发模式（首次编译约 3–5 分钟）
.\build.ps1        # 发布构建（首次约 5–15 分钟）
```

**方式二：手动命令**

```powershell
pnpm install
pnpm tauri dev     # 开发模式
pnpm tauri build   # 发布构建
```

**构建产物**

安装程序位于 `src-tauri\target\release\bundle\nsis\`（`.exe`），双击即可安装。

> 提示：若终端找不到 `cargo`，先执行 `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"`（脚本已自动处理）。

### 首次启动

1. 启动应用后，系统会引导填写 OpenAI 兼容的 API 端点与模型
2. 配置完成后即可开始对话
3. 可在设置中调整人格、主题等配置

---

## ⚙️ 配置说明

### API 配置

在设置页面或 `config.json` 中配置：

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

### 人格配置

人格文件位于 `personas/` 目录，支持 YAML 格式：

```yaml
id: "konata-default"
name: "此方（こなた）"
version: "1.0.0"

system_prompt: |
  你是"此方"（こなた），来自《幸运星》的少女。
  你性格开朗、热爱动漫和游戏，说话时经常引用动漫台词。
  你称呼用户为"{user_nickname}"。

personality:
  traits: ["活泼", "宅属性", "幽默", "偶尔毒舌"]
  speech_style: "口语化，带语气词，偶尔夹杂日语"
  interests: ["动画", "游戏", "Cosplay", "轻小说"]
```

### 工具配置

在设置 → 工具中可配置：
- 工具模式（只读/完全/自定义）
- 命令白名单
- 工作区路径
- 子代理配置

---

## 📂 项目结构

```
kagami-no-konata/
├── src/                       # React 前端
│   ├── components/            # UI 组件（chat/float/settings/persona/onboarding）
│   ├── stores/                # Zustand 状态管理
│   ├── types/                 # TypeScript 类型定义
│   └── utils/                 # 工具函数
│
├── src-tauri/                 # Rust 后端
│   ├── src/
│   │   ├── agent/             # Agent 模块（聊天/路由/工具运行时）
│   │   ├── commands/          # Tauri IPC 命令（60+ 个）
│   │   ├── llm/               # LLM 代理层（OpenAI 兼容/模型路由）
│   │   ├── memory/            # 记忆系统（提取/检索）
│   │   ├── persona/           # 人格引擎（YAML 加载/变量插值）
│   │   ├── store/             # 数据持久化（SQLite）
│   │   ├── config/            # 配置管理
│   │   └── mcp/               # MCP 服务器桥接
│   └── personas/              # 预置人格文件
│
├── public/                    # 静态资源（Live2D 模型/桌宠素材）
├── ARCHITECTURE.md            # 架构设计文档
└── CLAUDE.md                  # AI 辅助开发指南
```

---

## 🛠 开发指南

### 开发命令

```bash
# 安装依赖
pnpm install

# 开发模式（前端热重载 + 后端编译）
pnpm tauri dev

# 前端构建（类型检查 + Vite 打包）
pnpm build

# Rust 代码检查
cd src-tauri && cargo clippy

# Rust 测试
cd src-tauri && cargo test
```

### 开发环境配置

1. **IDE 推荐**：VS Code + rust-analyzer + Tauri 插件
2. **调试**：使用 `pnpm tauri dev` 启动开发模式
3. **日志**：Rust 后端日志输出到控制台

### 代码规范

- **前端**：遵循 ESLint 配置
- **后端**：遵循 `cargo clippy` 规范
- **提交**：确保 `cargo clippy`、`cargo test` 与 `pnpm build` 均通过

---

## 🤝 贡献指南

欢迎提交 Issue 与 Pull Request！

### 贡献流程

1. Fork 本仓库
2. 创建特性分支：`git checkout -b feature/your-feature`
3. 提交更改：`git commit -m 'Add some feature'`
4. 推送分支：`git push origin feature/your-feature`
5. 提交 Pull Request

### 提交规范

- 确保代码通过所有检查
- 添加必要的测试
- 更新相关文档
- 使用清晰的提交信息

---

## 🔐 数据与隐私

- **本地存储**：所有数据保存在本机应用数据目录
  - Windows：`%APPDATA%/com.konata-mirror.main/`
  - Linux：`~/.local/share/com.konata-mirror.main/`
- **隐私保护**：数据不会上传到任何第三方；唯一的外部请求来自你自己配置的 LLM / Embedding 端点
- **API Key 安全**：以明文存放在本地 `config.json`，请勿将其提交到版本库
- **工具安全**：命令工具不经 shell 执行，参数必须落在工作区内；文件改动会先快照，可随时回滚

---

## 📄 许可证

本项目基于 [MIT License](LICENSE) 开源。

Copyright (c) 2026 Kagami no Konata Contributors

---

## 🎨 致谢

### 第三方资源

- **Live2D Cubism 运行时**（`public/live2dcubismcore.min.js`、`public/live2d.min.js`）：© Live2D Inc.，属于其许可协议中的 "Redistributable Code"，随本应用按 [Live2D Proprietary Software License Agreement](https://www.live2d.com/eula/live2d-proprietary-software-license-agreement_en.html) 分发。
- **pixi.js**（`public/pixi.min.js`，v7.3.2）与 **pixi-live2d-display**（`public/cubism4.min.js`）：MIT License。
- **角色与模型素材**（`public/live2d/konata/`、`public/pet/`）：仅供学习与个人使用，版权归原作者及权利人所有；如权利人提出要求，我们会立即移除相关内容。

> 本项目是非官方粉丝作品，与《幸运星》版权方及 Live2D Inc. 均无关联。

### 相关项目

- [Tauri](https://tauri.app/) - 构建跨平台桌面应用
- [React](https://react.dev/) - 用户界面库
- [Live2D](https://www.live2d.com/) - 2D 角色动画技术

---

<p align="center">
  Made with ❤️ by Kagami no Konata Contributors
</p>
