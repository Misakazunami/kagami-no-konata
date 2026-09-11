# Konata_Mirror (镜中此方)

Konata_Mirror 是一个基于 **Tauri v2 + Rust** 后端与 **React 19 + TypeScript** 前端构建的桌面伴侣型 AI 助理。它结合了个性化角色扮演（Persona）、长期记忆提取检索系统（Long-term Memory）、悬浮窗桌面宠物（Live2D / 戳一戳互动）以及**多智能体协作与意图路由系统（Multi-Agent Routing Pipeline）**。

---

## 🌟 核心特性

- **🎭 角色扮演与人格系统（Persona Engine）**：内置并支持用户自定义 YAML 人格配置，支持变量插值、人设注入与动态热重载。
- **🧠 长期记忆系统（Memory System）**：
  - 基于语义向量嵌入（Embedding）的 Cosine 相似度检索，结合重要性权重评分（`similarity * 0.7 + importance * 0.3`）。
  - 对话异步提取与去重更新机制（事实、偏好、经历、情感四维标签）。
- **🤖 多智能体意图路由系统（Multi-Agent Router & Dispatcher）**：
  - **四级意图决策流水线**：
    - Level 1: 确定性斜杠前缀指令（如 `/sys`、`/system`），具备词边界安全保护与 0ms 零延迟分发。
    - Level 2: 预编译高置信正则表达式匹配（如 `^查看\s*(系统信息|系统状态|内存|cpu)`）。
    - Level 3: 启发式多关键词加权打分（饱和阈值与相关率综合评分）。
    - Level 4: 默认无缝回退到主对话智能体 `ChatAgent`。
  - **人设包裹模式（Persona Wrapping）**：支持将工具型智能体的输出以特定角色的语气进行转述，兼顾实用性与角色扮演沉浸感。
  - **思考流状态感知**：后台智能体执行时即时向思考流输出进度通知，杜绝界面白屏等待。
  - `SystemAgent` 现在**真的调用工具**（`get_current_time` / `get_system_info`），不再返回"已收到指令"这类假装执行的模板文本。
- **🛠 工具运行时（Tool Harness）**：让模型在生成过程中调用真实工具，而不是靠提示词假装。
  - **16 个内置工具**：文件读取/列目录/glob/内容搜索、时间与系统信息、应用状态、记忆检索与写入、人格查看、文件写入与精确编辑、命令执行、网页抓取、用系统程序打开文件。
  - **三档模式**：只读 / 标准（默认）/ 完整。只读模式直接移除写入类工具，命令执行只在完整模式下可见。
  - **工作区沙箱**：默认工作区固定在应用数据目录下的 `workspace/`，设置页可添加多个自定义根目录（新增根默认只读）；支持 `工作区id:相对路径` 多根寻址。
  - **敏感命令拦截**：命令不经 shell 执行；`cmd`、`powershell`、`curl`、`rm`、`reg`、`schtasks`、`certutil` 等由**内置黑名单硬拦截**（写进允许列表也无效），解释器求值参数（`python -c` / `node -e`）一律拒绝，参数中的路径必须落在工作区内，子进程不继承 API Key。
  - **审批与可见性**：写文件、执行命令、联网都要在**主窗口**弹窗批准（支持"仅本次 / 本会话允许 / 拒绝"，超时即拒绝）；结果不做跨轮保留，工具返回内容被标记为不可信数据以防提示注入。
  - **桌面宠物只有聊天**：悬浮窗完全不接入工具，也收不到任何工具事件。
- **🐱 桌面宠物悬浮窗（Floating Widget）**：
  - 透明置顶悬浮窗，内置时钟、Live2D 渲染与悬浮对话气泡。
  - 戳一戳互动机制：连续点击情绪反应，支持预置台词与实时 LLM 反应智能切换（一次性反应，不写入聊天记录）。
- **⚡ 双模式窗口设计**：
  - **主窗口（Main Window，900×680）**：全功能聊天界面、会话管理、模型提供商配置、人格编辑与记忆库查看。
  - **悬浮窗（Float Window，220×340）**：轻量透明桌宠伴侣。

---

## 🛠️ 技术栈

- **前端**：React 19、TypeScript、Vite、Zustand、Pixi.js / Live2D
- **后端**：Tauri v2、Rust、Tokio（异步运行时）、Rusqlite（SQLite 存储，WAL 模式）、Reqwest（SSE 流式）
- **数据持久化**：应用数据统一存放于 `%APPDATA%/com.konata-mirror.main/`（`data.db`、`config.json`、`personas/`、`workspace/`）
  - 首次启动若检测到旧的 `com.konata-mirror.app` 数据目录，会自动**只复制不删除**地迁移一次聊天记录、配置、人格与工作区文件（旧目录原样保留）

---

## 🚀 快速开始

### 依赖环境

- [Node.js](https://nodejs.org/) (建议 LTS) & [pnpm](https://pnpm.io/)
- [Rust](https://rustup.rs/) (1.87+，代码使用了 `usize::is_multiple_of` 等较新的稳定 API)

### 安装依赖

```bash
pnpm install
```

### 开发模式启动

启动 Vite 开发服务器及 Tauri 编译后端：

```bash
pnpm tauri dev
```

> **提示**：首次编译 Rust 依赖约需 3-5 分钟，后续热重载启动仅需数秒。

### 项目构建与打包

编译前端代码与生成发行包（Windows 下生成 NSIS 安装包与 MSI）：

```bash
# 仅构建前端（类型检查 + Vite 打包）
pnpm build

# 全量构建发布包
pnpm tauri build
```

---

## 📂 项目结构

```text
Konata_Mirror/
├── src/                      # 前端 React 源代码
│   ├── components/
│   │   ├── chat/             # 消息列表、输入框、流式文本
│   │   ├── float/            # 悬浮窗、Live2D、戳一戳、时钟
│   │   ├── persona/          # 人格编辑器
│   │   ├── settings/         # 模型与应用配置界面
│   │   └── onboarding/       # 引导初始化界面
│   ├── stores/               # Zustand 全局状态管理
│   └── App.tsx               # 根组件与窗口识别路由
├── src-tauri/                # 后端 Rust 源代码
│   ├── src/
│   │   ├── agent/            # Agent 抽象、Router 意图路由、Dispatcher 与各智能体实现
│   │   ├── commands/         # Tauri IPC 命令注册（chat、settings、persona、memory 等）
│   │   ├── llm/              # LLM 客户端与 SSE 流式代理
│   │   ├── memory/           # 记忆提取与向量检索
│   │   ├── persona/          # 人格配置解析与提示词组装
│   │   └── store/            # SQLite 数据库管理与迁移
│   └── Cargo.toml            # Rust 依赖配置
├── ARCHITECTURE.md           # 架构设计演进文档
└── CLAUDE.md                 # 辅助开发规范指南
```

---

## 📄 开源许可证

本项目采用 [MIT License](LICENSE) 开源。
