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

> 当前版本 `0.1.0-beta.1`，处于 Beta 阶段，配置项与交互仍可能调整。

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
  - [API 与模型配置](#api-与模型配置)
  - [人格配置](#人格配置)
  - [记忆配置](#记忆配置)
  - [工具与安全配置](#工具与安全配置)
  - [界面与窗口显示](#界面与窗口显示)
- [内置工具清单](#-内置工具清单)
- [权限模型与安全边界](#-权限模型与安全边界)
- [项目结构](#-项目结构)
- [开发指南](#-开发指南)
- [贡献指南](#-贡献指南)
- [数据与隐私](#-数据与隐私)
- [许可证](#-许可证)
- [致谢](#-致谢)

---

## ✨ 功能特性

### 🎭 角色扮演系统
- 内置与自定义 YAML 人格配置，同 ID 时磁盘文件覆盖内置人格
- 变量插值（如 `{user_nickname}`）
- 人设注入与热重载：保存 / 删除 / 导入人格后立即生效
- 多人格切换；普通会话与任务会话共用同一人格
- 每个角色可定义侧栏短名、UI 头像与「戳一戳」台词

### 🧠 长期记忆
- 四种记忆类型：事实 / 偏好 / 经历 / 情绪
- 向量检索，打分公式 `相似度 × 0.7 + 重要性 × 0.3`
- 回复结束后异步提取事实与偏好，按 `新增 / 更新 / 跳过` 去重合并
- 跨会话持久化；每轮最多注入 5 条（可配置）
- 历史遗留的无向量记忆按重要性兜底参与排序，不会变成「看得见却检索不到」的数据

### 🛠 工具执行
- **26 个内置工具**（见 [内置工具清单](#-内置工具清单)），覆盖文件读写、内容检索、命令执行、桌面控制、网页抓取、记忆与人格查询等
- 三档权限模式（只读 / 标准 / 完全），并按会话类型自动决定工具集与轮数预算
- 多工作区沙箱隔离：路径只能落在许可的根目录内，防 `..` 与符号链接逃逸
- 命令不经 shell 执行，敏感程序与「代执行」参数硬拦截（用户白名单也无法放行）
- 同一轮内的多个只读调用并行执行，只消耗一轮工具预算
- 写操作逐次审批，写前自动快照，可随时回滚；删除默认进系统回收站
- 工具输出以「不可信数据」标记回灌模型，削弱提示注入的影响

### 🖥 屏幕感知与桌面操作
- 任务 Work（完全模式）下进入「OS 接管」：每轮自动把当前显示器截图放进上下文，模型不需要自己截图
- `os_control` 支持鼠标点击 / 双击、键盘输入、组合键、把窗口切到前台（窗口激活目前仅 Windows）
- 截图优先跟随鼠标所在显示器（取不到时退回主屏），缩放比与屏幕原点统一换算，多显示器与 DPI 缩放下落点仍然准确
- 截图缩放后以 JPEG 下发，长边上限与质量可配置；每轮只保留最新一张，旧截图从上下文清理
- 只有 `os_control` 可见时才自动截屏（Wayland 只读模式例外：显式 opt-in 后由 `look_at_screen` 按需截取）：普通聊天、任务 Plan 与悬浮窗都不会采集屏幕

### 🗂 任务模式
- 会话分「聊天」与「任务」两类；任务会话再分 Plan（规划）与 Work（执行）
- 计划面板：模型提交计划并逐项更新状态，用户可直接编辑与勾选
- 工作笔记：模型把跨轮结论记下来，过时结论可删除
- 文件改动面板：按生成批次查看快照、一键回滚，并附带任务报告
- 会话级 AUTO（仅任务会话可开）：跳过审批弹窗（包括桌面操作），硬性安全边界仍然生效；请只在信任当前会话时开启

### 🤖 多智能体与模型路由
- 斜杠指令 / 正则 / 关键词三级加兜底的四级意图流水线
- 工具型智能体的输出会以当前人设的语气转述，且提示词不写死任何角色名
- 只读子代理：把「分别看看 A/B/C 再汇总」的调查任务并行分发（每轮最多 2 个）
- 多提供商管理；会话级模型选择（跟随全局 / 手动指定 / 自动路由）
- 自动路由支持主模型与子模型池，子代理在池上轮转
- 深度思考开关按模型能力探测，支持显式声明覆盖

### 🔌 MCP 外部工具
- 通过 stdio JSON-RPC 连接用户显式启用并标记为可信的 MCP 服务器
- 外部工具以 `mcp:<服务器>:<工具>` 命名并入工具表，权限按声明映射
- 取消信任立即失效并结束对应子进程

### 💬 对话管理
- 任意消息的回退、重试与编辑（回退时可一并撤销文件改动）
- 单条 / 整会话一键复制（Markdown）
- 右侧消息时间轴快速跳转
- 会话自动命名、超长上下文摘要压缩、token 与耗时统计

### 🪟 界面与自适应布局
- 会话侧栏可一键收起 / 展开，偏好写入配置并在下次启动时恢复
- 窗口变窄（≤ 699px）时侧栏自动隐藏、按需唤出为覆盖层，不再靠挤压正文换取空间
- 三档显示密度（紧凑 / 标准 / 宽松）：侧栏宽度、正文留白、版心与面板高度预算随视口切换
- 宽屏下正文按版心居中（约 64–72rem），全屏时行宽不会拉成一条长带
- 顶部状态栏、底部计划 / 笔记 / 改动面板都有高度预算，矮窗口里优先保证消息区可用

### 🐱 桌面宠物
- 透明置顶悬浮窗，Live2D 渲染（眼神跟随、底部对齐）
- 时钟与「戳一戳」互动（一次性反应，不写入聊天记录）
- 悬浮窗内可直接对话，与主窗口状态实时同步

### 🪟 双窗口与桌面集成
- **主窗口**（900×680，可缩放，最小 600×400）：完整聊天与设置界面
- **悬浮窗**（220×340）：透明无边框、置顶、跳过任务栏
- 系统托盘菜单（打开主窗口 / 打开悬浮窗 / 设置 / 退出）
- 全局快捷键 `Ctrl+Shift+K` 切换主窗口（Wayland 下需设置 `KONATA_WAYLAND_GLOBAL_SHORTCUTS=1` 走 XDG 门户，实验）；关闭按钮行为可配置（隐藏 / 隐藏并显示桌宠 / 退出）
- 主窗口从最小尺寸拉到全屏都保持可用：侧栏可折叠、正文有版心、弹窗有高度上限与内部滚动

---

## 🧱 技术栈

| 层 | 技术 |
| --- | --- |
| 前端 | React 19 · TypeScript · Vite 7 · Zustand 5 · react-markdown + highlight.js · Pixi.js 6 / pixi-live2d-display |
| 后端 | Tauri v2 · Rust 2021 · Tokio · rusqlite（SQLite WAL）· reqwest（rustls + SSE 流式）· serde / serde_yaml · screenshots + enigo（屏幕感知与键鼠控制） |
| 安全 | 路径监狱（globset）· 命令守卫 · oneshot 审批通道 · 写前快照 · 系统回收站 · 桌面操作审批 |
| 数据 | 应用数据目录下的 `data.db` / `config.json` / `personas/` / `workspace/` / `snapshots/` |

---

## 🚀 快速开始

### 环境要求

- **Node.js**：20.19+ 或 22.12+（Vite 7 的要求，推荐 22 LTS）
- **pnpm**：包管理器
- **Rust**：1.87+
- **系统依赖**（Linux）：见 [docs/linux.md](docs/linux.md)。需要 WebKitGTK 4.1、`libxdo-dev`（enigo 键鼠）、`libxcb1-dev` / `libxrandr-dev`（截图）、`libayatana-appindicator3-dev`（托盘）等；**推荐使用 X11/Xorg 会话**，Wayland 下桌面能力会降级（只读截图与门户快捷键需分别显式 opt-in）。

### 安装步骤

```bash
# 1. 克隆仓库
git clone https://github.com/your-username/kagami-no-konata.git
cd kagami-no-konata

# 2. 安装前端依赖
pnpm install

# 3. 开发模式启动（首次编译 Rust 约 3–5 分钟）
pnpm tauri dev

# 4. 发布构建（Windows 生成 NSIS 安装程序；Linux 见下文）
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

### Linux 构建

Linux 的构建依赖、安装包、Wayland 限制和故障排查详见 **[docs/linux.md](docs/linux.md)**。

快速命令：

```bash
# Ubuntu / Debian 构建依赖
sudo apt-get install -y \
  build-essential curl wget file \
  libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
  librsvg2-dev libxdo-dev libxcb1-dev libxrandr-dev \
  libdbus-1-dev libssl-dev patchelf rpm

pnpm install
./scripts/build-linux.sh              # deb + AppImage
./scripts/build-linux.sh deb appimage rpm
```

产物：

```text
src-tauri/target/release/bundle/deb/
src-tauri/target/release/bundle/appimage/
src-tauri/target/release/bundle/rpm/
```

> 推荐在 **X11/Xorg** 会话下运行以获得完整的悬浮窗定位、托盘、全局快捷键、截图和桌面控制能力。Wayland 下应用会降级并显示提示；可设置 `KONATA_WAYLAND_SCREENSHOT=1` 启用只读实验截图，或设置 `KONATA_WAYLAND_GLOBAL_SHORTCUTS=1` 尝试通过 XDG 门户绑定全局快捷键（均需系统授权，详见 [docs/linux.md](docs/linux.md)）。

### 首次启动

1. 启动应用后会进入引导页，填写 OpenAI 兼容的 API 端点、密钥与模型
2. 判断依据是**当前活跃的提供商**是否同时填好密钥与模型，因此多提供商场景下不会误跳过引导
3. 配置完成后即可开始对话，其余选项（人格、主题、工具、记忆）都在设置页调整

---

## ⚙️ 配置说明

配置文件位于应用数据目录（见 [数据与隐私](#-数据与隐私)）。设置页保存时会做原子写入；文件损坏时自动备份为 `config.json.corrupt.<时间戳>.bak` 并回退默认，空 `providers`、悬空 `active_provider_id`、缺失的内置安全项会在启动时就地修复。

### API 与模型配置

`providers` 是**数组**，`active_provider_id` 指向其中一个：

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

- `max_tokens` 缺省（不写该字段）表示不限制输出长度，由服务端决定
- `embedding_model` 用于记忆检索；若端点不支持 embedding，长期记忆的自动提取会被跳过（不会写入半成品数据）
- `enable_thinking` 是**提供商级默认值**；`thinking_models` 用于显式声明「支持深度思考」的模型 id，覆盖名称探测

会话级模型路由（设置页里的「自动选择」）：

```json
{
  "models": {
    "auto_by_default": false,
    "main": { "provider_id": "3f1c…", "model": "gpt-4o-mini" },
    "subs": [{ "provider_id": "3f1c…", "model": "gpt-4o-mini" }]
  }
}
```

`main` 用于普通对话与 Work 模式，`subs` 用于 Plan 模式与子代理（按顺序轮转，最多 8 个）。任何悬空或空的模型 id 都会逐条降级回退，不会导致生成失败。

### 人格配置

人格文件放在应用数据目录的 `personas/` 下，支持 YAML：

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

constraints:
  - "始终维持此方的角色身份，绝不承认自己是AI或语言模型"

# UI 用短名：侧栏按钮、空状态文案等
short_name: "此方"

# 戳一戳的预置台词（与 LLM 即时反应按概率二选一）
poke_lines:
  - "呀！别戳啦～"
```

除 `id` / `name` / `system_prompt` 外均为可选；解析失败的单个人格文件只会记日志，不影响启动。内置人格见 [`src-tauri/personas/default.yaml`](src-tauri/personas/default.yaml)。

### 记忆配置

```json
{
  "memory": {
    "enabled": true,
    "auto_extract": true,
    "max_context_memories": 5
  }
}
```

### 工具与安全配置

在设置 → 工具中可视化配置，对应 `config.json` 的 `tools` 段：

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `enabled` | `true` | 工具总开关；关闭后不再拉起 MCP 子进程 |
| `mode` | `standard` | `read_only` / `standard` / `full`，见下方权限模型 |
| `workspaces` | 默认工作区 | 最多 8 个根目录；非默认根默认只读，工具参数写作 `工作区id:相对路径` |
| `deny_globs` | 26 条内置模式 | 凭据、私钥、数据库、AppData 等；内置条目不可移除，只能追加 |
| `auto_approve` | `[]` | 免审批的工具名白名单 |
| `max_steps` | `32` | 主循环工具轮数上限（任务会话至少 20 轮） |
| `max_output_bytes` | `65536` | 单次工具输出上限，超出按头尾截断 |
| `approval_timeout_secs` | `120` | 审批等待上限，超时按拒绝处理 |
| `call_timeout_secs` | `180` | 单次工具调用超时（编译类命令需要） |
| `screenshot_max_dim` | `1024` | 任务 Work 的 OS 接管模式下，送进模型的屏幕截图长边上限；`0` = 不缩放，按原始分辨率下发（看清小字用，token 会变多） |
| `screenshot_jpeg_quality` | `85` | 截图 JPEG 质量（40–100）。分辨率调到 `0` 时建议提到 90 以上 |
| `working_memory` | `true` | 工作笔记：模型主动记下的跨轮结论 |
| `subagent_max_children` / `subagent_steps` / `subagent_max_tasks` | `2` / `32` / `3` | 只读子代理的并发、轮数与单次任务数 |
| `subagent_timeout_secs` | `600` | 子代理批次整体时间预算 |
| `command_allowlist` | 87 个常用开发程序 | 只能追加；内置黑名单优先级更高，无法被放行 |
| `web_domain_allowlist` | `[]` | `web_fetch` 允许的域名；为空即禁用网页抓取 |
| `search.enabled` | `false` | 联网检索（唯一会把提问文本发给第三方的能力，默认关闭） |
| `search.provider` | `searxng` | `searxng`（推荐自建）/ `tavily` / `brave` |
| `mcp.servers` | `[]` | 外部 MCP 服务器列表，需显式启用并标记可信 |

### 界面与窗口显示

对应 `config.json` 的 `ui` 段；主题、字号与桌宠相关项在设置 → 外观中可视化调整，其余由界面操作直接写入。

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `theme` | `dark` | `dark` / `light` / `auto`（跟随系统） |
| `font_size` | `14` | 根字号（12–18px）。全站尺寸都用 rem，调它即整体缩放界面 |
| `show_message_stats` | `false` | 消息下方显示 token 数与耗时 |
| `sidebar_collapsed` | `false` | 会话侧栏是否收起。由顶栏的折叠按钮写入，下次启动沿用 |
| `close_action` | `hide` | 点关闭按钮的行为：`hide` / `hide_and_float` / `exit` |
| `float_position` | `bottom-right` | 悬浮窗吸附位置（六个角落） |
| `show_float_clock` | `false` | 悬浮窗内显示时钟 |
| `poke_enabled` | `true` | 启用「戳一戳」互动 |
| `poke_probability` | `0.3` | 戳一下触发反应的概率 |
| `poke_llm_chance` | `0.15` | 反应由 LLM 即时生成（而非预置台词）的概率 |
| `bubble_auto_hide_secs` | `20` | 气泡自动隐藏秒数；`0` 表示不自动隐藏（此时靠点击气泡关闭） |

主窗口自适应按**视口宽度**分三档，断点只在 `src/styles/chat.css` 里定义一处：

| 档位 | 视口宽度 | 行为 |
| --- | --- | --- |
| 紧凑 | < 700px | 侧栏自动隐藏（顶栏按钮唤出为覆盖层）、正文留白收窄、顶栏次要状态收进「详情」、时间轴隐藏 |
| 标准 | 700–1399px | 侧栏 240px，正文版心 64rem |
| 宽松 | ≥ 1400px | 侧栏 280px，正文两侧留白加大，版心 72rem |

窗口高度 ≤ 560px 时另有一条纵向压缩档：底部面板栈高度预算收紧、气泡内边距减小，保证消息区至少能显示约 4 行。侧栏的自动隐藏是**临时状态**，不写入配置——用户在宽窗口手动收起后，拉窄再拉宽会恢复该偏好，不会被自动展开覆盖。

弹窗（确认框 / 任务创建 / 审批 / 回退）统一有视口约束：高度上限 `min(90vh, 760px)`、内容区独立滚动、动作按钮常驻底部。因此把窗口拖到最小尺寸时，弹窗里「取消 / 确定」依然点得到，不会被推出可视区。

---

## 🧰 内置工具清单

| 权限 | 工具 | 用途 |
| --- | --- | --- |
| 只读 | `read_file` | 读取工作区文本文件（带行号、可分页，支持一次读多个路径） |
| 只读 | `list_dir` | 列出目录内容 |
| 只读 | `glob_search` | 按通配符查找文件 |
| 只读 | `grep_search` | 按正则搜索文件内容 |
| 只读 | `get_current_time` | 当前本地时间与日期 |
| 只读 | `get_system_info` | 操作系统与硬件概况 |
| 只读 | `get_app_status` | 应用状态（会话数、记忆条数等） |
| 只读 | `search_memory` | 检索长期记忆 |
| 只读 | `list_personas` | 列出可用人格 |
| 只读 | `read_persona` | 读取某个人格的完整定义 |
| 只读 | `spawn_subagents` | 派发 1–3 个只读调查子代理并汇总结论 |
| 应用内写入 | `save_memory` | 写入长期记忆（免审批） |
| 会话级写入 | `update_plan` | 提交 / 更新任务计划（免审批，只读模式仍可见） |
| 会话级写入 | `save_note` | 记下跨轮结论（免审批） |
| 会话级写入 | `forget_note` | 删除过时或错误的笔记（免审批） |
| 工作区写入 | `write_file` | 新建或覆盖文件（需审批，写前快照） |
| 工作区写入 | `edit_file` | 精确替换文件片段（需审批，写前快照） |
| 工作区写入 | `delete_path` | 删除文件或目录（需审批，默认进回收站） |
| 工作区写入 | `move_path` | 移动 / 重命名（需审批） |
| 工作区写入 | `copy_path` | 复制（需审批） |
| 工作区写入 | `open_with_system` | 用系统默认程序打开文件或白名单链接（需审批） |
| 命令执行 | `run_command` | 不经 shell 执行命令，支持多步串行（需审批，仅「完全」模式可见） |
| 系统控制 | `os_control` | 鼠标点击 / 双击、键盘输入、组合键、切换前台窗口（需审批，仅「完全」模式可见；窗口激活目前仅 Windows） |
| 系统控制 | `look_at_screen` | Wayland 只读模式下按需截图（仅查看，不能控制键鼠；需显式 opt-in） |
| 网络 | `web_fetch` | 抓取网页为纯文本（需审批 + 域名白名单） |
| 网络 | `web_search` | 联网检索候选链接（需审批 + 配置检索后端） |

> `run_command` 没有 shell：不支持管道、重定向与 `&&`；需要连续执行时用 `steps` 数组一次提交，按顺序串行执行且只需一次审批。

> 任务 Work 模式下，系统会在每轮生成前自动注入当前显示器截图（仅当 `os_control` 可见）；模型不需要也不能调用截图工具。Wayland 只读模式（`KONATA_WAYLAND_SCREENSHOT=1`）例外：`os_control` 不可用，模型可调用 `look_at_screen` 按需截图，图片在下一轮对话前注入。

---

## 🔐 权限模型与安全边界

工具的实际可用范围由**会话类型**决定，而不是只看全局开关：

| 会话类型 | 工具模式 | 轮数预算 | 说明 |
| --- | --- | --- | --- |
| 普通聊天 | 只读 | 3 | 写入、命令、联网、桌面控制均不可见 |
| 任务 · Plan | 只读 + 联网检索 | ≥20 | 只能读文件并联网查资料，每次调用仍需审批 |
| 任务 · Work | 完全 | ≥20 | 命令与桌面控制可见，但每次写入 / 执行 / 桌面操作仍需审批 |
| 悬浮窗 | 不使用工具 | — | 纯对话，避免桌宠窗口触发任何操作 |

> 只有任务 Work 会进入 OS 接管模式并**自动**采集屏幕；Wayland 只读模式（`KONATA_WAYLAND_SCREENSHOT=1`）下，模型可在允许只读工具的模式按需调用 `look_at_screen`。普通聊天、任务 Plan 与悬浮窗都不自动截屏。`os_control` 不受路径监狱与写前快照覆盖，它唯一的闸门是「仅完全模式可见 + 逐次审批」；会话 AUTO 开启后会跳过该审批。`look_at_screen` 是只读工具，不需要审批，但只在显式 opt-in 的 Wayland 会话可见。

在此之上还有五层彼此独立的防线：

1. **可见性**：不可见的工具既不会进入模型 schema，按名调用也会被拒绝。
2. **审批**：写入、命令、网络与桌面操作四类工具逐次请求用户批准；超时、连接中断或无法识别的决定一律按**拒绝**处理（fail-closed）。点「停止生成」会立刻把该轮所有待审批按拒绝结束。
3. **路径监狱**：所有字符串转路径都必须经过同一处校验——逐段消解 `..`、canonicalize 后复检根目录（因此指向外部的符号链接被拦下）、拒绝 `\\?\`/UNC/NTFS 数据流/设备名；`config.json`、`*.db`、`.ssh`、`.env`、私钥等 26 条模式默认不可读写。
4. **命令守卫**：黑名单（shell、计划任务、注册表、网络下载器、`npx`/`bunx` 这类「下载即执行」）优先于用户白名单；参数层继续封堵代执行通道（`find -exec`、`fd -x`、`rg --pre`、`git -c alias.*`、`npm exec`/`pnpm dlx`、解释器求值参数），参数里的路径必须落在工作区内，子进程环境按白名单过滤（KEY / TOKEN / SECRET 不外泄）。
5. **可回滚**：改动前把原文件复制到快照目录（单文件 4MB、单批 64MB 上限），过期快照启动时自动回收；删除默认走系统回收站，跨卷时退化为「复制后删除」，任一步失败则不动原文件。

此外，工具结果对模型标注为 `untrusted`（是数据不是指令），记忆与笔记注入前会转义尖括号，防止内容里的标签提前闭合边界。

---

## 📂 项目结构

```
kagami-no-konata/
├── src/                       # React 前端（51 个源文件，约 1.5 万行）
│   ├── components/
│   │   ├── chat/              # 聊天窗口、消息气泡、流式渲染、工具卡片、审批、计划、变更、笔记、时间轴
│   │   ├── float/             # 悬浮窗、Live2D 画布、时钟、戳一戳
│   │   ├── settings/          # 设置页（用户信息 / 提供商 / 模型 / 记忆 / 工具 / 外观 / 统计）
│   │   ├── persona/           # 人格编辑器
│   │   ├── onboarding/        # 首次启动引导
│   │   └── ui/                # 错误边界、确认框、Toast
│   ├── hooks/                 # 模态焦点陷阱、媒体查询订阅（侧栏自动折叠用）
│   ├── stores/                # Zustand：chatStore（会话与生成）/ uiStore
│   ├── types/                 # 事件、模型、人格、工具的类型定义
│   ├── utils/                 # 剪贴板、会话导出、消息文本
│   └── styles/                # tokens / base / chat / tools / panels / conversation / float / settings / a11y
│
├── src-tauri/                 # Rust 后端（67 个源文件，约 2.9 万行）
│   ├── src/
│   │   ├── agent/             # 智能体：chat_agent / dispatcher / router / system_agent
│   │   │   └── harness/       # 工具运行时：registry / runner / approve / jail / screen
│   │   │                      #   command_guard / snapshot / subagent / trash / tools/
│   │   │                      #   （tools/ 含 os_control 等 26 个内置工具）
│   │   ├── commands/          # 70 个 Tauri IPC 命令（chat/settings/tools/persona/memory/backup/stats/window）
│   │   ├── llm/               # OpenAI 兼容客户端、SSE 解析、模型路由、能力探测
│   │   ├── memory/            # 记忆提取器
│   │   ├── persona/           # 人格引擎（YAML 加载 / 变量插值）
│   │   ├── store/             # SQLite 持久化（db / chat_store / memory_store）
│   │   ├── config/            # 配置读写与自愈迁移
│   │   ├── mcp/               # MCP 服务器桥接（stdio JSON-RPC）
│   │   └── appdata.rs         # 应用数据目录与旧目录迁移
│   ├── personas/              # 预置人格文件
│   └── capabilities/          # 前端权限声明（窗口 / 托盘 / 全局快捷键 / opener）
│
└── public/                    # 静态资源（Live2D 模型、桌宠素材、Cubism 运行时）
```

---

## 🛠 开发指南

### 开发命令

```bash
# 安装依赖
pnpm install

# 开发模式（前端热重载 + 后端编译，Vite 固定端口 1420）
pnpm tauri dev

# 前端构建（类型检查 + Vite 打包）
pnpm build

# Rust 代码检查
cd src-tauri && cargo clippy --all-targets -- -D warnings

# Rust 测试
cd src-tauri && cargo test
```

### 测试与 CI

- 后端单元测试密度较高，测试名即规格：空回合重试、输出截断、并行只读调用、步数耗尽、审批拒绝与放行、路径逃逸、命令守卫、SSE 分片解析、迁移幂等等都有对应用例
- CI 在 Linux 上跑 `pnpm build` + `cargo test` + `cargo clippy --all-targets -- -D warnings`
- CI 在 Windows 上跑 `cargo check --all-targets` + `cargo clippy`，确保 Windows 专属分支（cmd 包装、系统回收站、路径规范化）被真正编译到

### 开发环境配置

1. **IDE 推荐**：VS Code + rust-analyzer + Tauri 插件
2. **调试**：使用 `pnpm tauri dev` 启动开发模式（Rust 后端日志输出到控制台）
3. **注意**：CSP 在开发与生产两套配置下不同，新增外部资源时需要同时检查 `src-tauri/tauri.conf.json`

### 代码规范

- **前端**：TypeScript 严格模式（`noUnusedLocals` / `noUnusedParameters`）
- **后端**：遵循 `cargo clippy` 规范
- **提交**：确保 `cargo clippy`、`cargo test` 与 `pnpm build` 均通过

---

## 🤝 贡献指南

欢迎提交 Issue 与 Pull Request！

### 贡献流程

1. Fork 本仓库
2. 创建特性分支：`git checkout -b feature/your-feature`
3. 提交更改：`git commit -m 'Add some feature'`
4. 推送分支：`git push origin your-feature`
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
  - 目录内容：`data.db`（SQLite，WAL 模式）、`config.json`、`personas/`、`workspace/`、`snapshots/`
- **隐私保护**：数据不会上传到任何第三方。唯一的外部请求来自你自己配置的 LLM / Embedding 端点，以及你显式开启的联网检索后端与 MCP 服务器（联网检索默认关闭）
- **屏幕截图**：任务 Work 模式每轮会把当前显示器截图（JPEG，分辨率与质量可配）随对话请求发给所配置的 LLM 端点；截图可能包含屏幕上的其他窗口内容。普通聊天、任务 Plan 与悬浮窗不截图，关闭工具或切出 Work 后立即停止。Wayland 下默认不截图；只有设置 `KONATA_WAYLAND_SCREENSHOT=1` 后才启用只读截图，且仅在模型调用 `look_at_screen` 时按需截取
- **API Key 安全**：以明文存放在本地 `config.json`，请勿将其提交到版本库；该文件同时在内置拒绝清单中，工具无法读取它
- **工具安全**：命令工具不经 shell 执行，参数必须落在工作区内；文件改动会先快照，可随时回滚；删除默认进系统回收站；桌面操作逐次审批且只在任务 Work 中可见
- **数据迁移**：应用标识曾发生过变更，启动时会从旧目录**只复制、不删除**地迁移一次聊天记录、配置、人格与工作区

---

## 📄 许可证

本项目基于 [MIT License](LICENSE) 开源。

Copyright (c) 2026 Kagami no Konata Contributors

---

## 🎨 致谢

### 第三方资源

- **Live2D Cubism 运行时**（`public/live2dcubismcore.min.js`、`public/live2d.min.js`）：© Live2D Inc.，属于其许可协议中的 "Redistributable Code"，随本应用按 [Live2D Proprietary Software License Agreement](https://www.live2d.com/eula/live2d-proprietary-software-license-agreement_en.html) 分发。
- **pixi.js**（v6.5.6）与 **pixi-live2d-display**（v0.4.0）：通过 npm 依赖引入，MIT License。
- `public/pixi.min.js` 与 `public/cubism4.min.js`：上述两个库的历史打包副本，当前未被 `index.html` 引用。
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
