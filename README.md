<p align="center">
  <img src="public/pet/default.png" width="140" alt="Kagami no Konata 桌面宠物" />
</p>

<h1 align="center">Kagami no Konata（镜中此方）</h1>

<p align="center">
  <a href="https://v2.tauri.app/"><img src="https://img.shields.io/badge/Tauri-v2-24C8DB?logo=tauri&logoColor=white" alt="Tauri v2" /></a>
  <a href="https://react.dev/"><img src="https://img.shields.io/badge/React-19-61DAFB?logo=react&logoColor=white" alt="React 19" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-2021-000000?logo=rust&logoColor=white" alt="Rust" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-green.svg" alt="MIT License" /></a>
</p>

<p align="center">本地优先的桌面 AI 伴侣：角色扮演对话、长期记忆、可审批的工具执行，以及一只住在悬浮窗里的桌宠。</p>

---

## ✨ 功能

- **🎭 角色扮演** — 内置与自定义 YAML 人格，支持变量插值、人设注入与热重载。
- **🧠 长期记忆** — 向量检索（`相似度 × 0.7 + 重要性 × 0.3`），对话异步提取事实与偏好并去重更新。
- **🛠 工具执行** — 16 个内置工具（文件读写、内容搜索、命令执行、网页抓取等）：三档权限、工作区沙箱、敏感命令硬拦截、写操作逐次审批并可回滚。
- **🤖 多智能体路由** — 斜杠指令 / 正则 / 关键词四级意图流水线；工具型智能体的输出可按人设语气转述。
- **💬 对话管理** — 任意消息的回退、重试与编辑，单条 / 整会话一键复制（Markdown），右侧消息时间轴快速跳转。
- **🐱 桌面宠物** — 透明置顶悬浮窗，Live2D 渲染、时钟与「戳一戳」互动（一次性反应，不写入聊天记录）。
- **🪟 双窗口** — 主窗口（900×680）负责完整聊天与设置；悬浮窗（220×340）是轻量伴侣。

## 🧱 技术栈

| 层 | 技术 |
| --- | --- |
| 前端 | React 19 · TypeScript · Vite · Zustand · Pixi.js / Live2D |
| 后端 | Tauri v2 · Rust · Tokio · rusqlite（SQLite WAL）· reqwest（SSE 流式） |
| 数据 | 应用数据目录下的 `data.db` / `config.json` / `personas/` / `workspace/` |

## 🚀 快速开始

**环境要求**：Node.js（LTS）+ pnpm、Rust 1.87+。Linux 还需安装 [Tauri 系统依赖](https://v2.tauri.app/start/prerequisites/)（WebKitGTK 等）。

```bash
pnpm install        # 安装前端依赖
pnpm tauri dev      # 开发模式（首次编译 Rust 约 3–5 分钟）
pnpm tauri build    # 发布构建（Windows 生成 NSIS / MSI）
```

Rust 侧检查与测试：

```bash
cd src-tauri
cargo clippy
cargo test
```

首次启动会引导填写 OpenAI 兼容的 API 端点与模型，随后即可开始对话。

## 🔐 数据与隐私

- 数据全部保存在本机应用数据目录（Windows：`%APPDATA%/com.konata-mirror.main/`，Linux：`~/.local/share/com.konata-mirror.main/`），不会上传到任何第三方；唯一的外部请求来自你自己配置的 LLM / Embedding 端点。
- API Key 以明文存放在本地 `config.json`，请勿将其提交到版本库。
- 命令工具不经 shell 执行，参数必须落在工作区内；文件改动会先快照，可随时回滚。

## 📂 项目结构

```text
src/                 前端：components（chat / float / settings / persona）、stores、utils、types
src-tauri/           后端：agent（工具运行时与路由）、commands、llm、memory、persona、store
ARCHITECTURE.md      架构设计与关键取舍
CLAUDE.md            面向 AI 辅助开发的仓库约定
```

## 🤝 参与贡献

欢迎提交 Issue 与 Pull Request。提交前请确保 `cargo clippy`、`cargo test` 与 `pnpm build` 均通过。

## 🎨 素材与第三方许可

- **Live2D Cubism 运行时**（`public/live2dcubismcore.min.js`、`public/live2d.min.js`）：© Live2D Inc.，属于其许可协议中的 "Redistributable Code"，随本应用按 [Live2D Proprietary Software License Agreement](https://www.live2d.com/eula/live2d-proprietary-software-license-agreement_en.html) 分发。
- **pixi.js**（`public/pixi.min.js`，v7.3.2）与 **pixi-live2d-display**（`public/cubism4.min.js`）：MIT License。
- **角色与模型素材**（`public/live2d/konata/`、`public/pet/`）：仅供学习与个人使用，版权归原作者及权利人所有；如权利人提出要求，我们会立即移除相关内容。

> 本项目是非官方粉丝作品，与《幸运星》版权方及 Live2D Inc. 均无关联。

## 📄 许可证

本项目基于 [MIT License](LICENSE) 开源。
