# Kagami no Konata（镜中此方）— 前期开发框架与技术栈方案

> 版本：v0.1-draft | 日期：2026-06-05

---

## 一、系统架构总览

```
┌─────────────────────────────────────────────────────────────────────┐
│                     Kagami no Konata 桌面应用                        │
│                                                                     │
│  ┌──────────────┐   ┌──────────────┐   ┌──────────────────────────┐ │
│  │  ChatWindow   │   │ FloatWidget  │   │   Onboarding / Settings  │ │
│  │  (主交互界面)  │   │ (桌面悬浮窗)  │   │     (首次启动引导)        │ │
│  └──────┬───────┘   └──────┬───────┘   └────────────┬─────────────┘ │
│         │                  │                         │               │
│  ═══════╪══════════════════╪═════════════════════════╪═══════════    │
│         │        Frontend (React + TypeScript)       │               │
│  ═══════╪══════════════════╪═════════════════════════╪═══════════    │
│         │                  │                         │               │
│         └──────────┬───────┘                         │               │
│                    │  Tauri IPC (invoke/event)       │               │
│  ──────────────────┼─────────────────────────────────┼────────────── │
│                    │        Backend (Rust / Tauri)    │               │
│  ┌─────────────────▼─────────────────────────────────▼────────────┐ │
│  │                      Command Dispatcher                        │ │
│  │            (统一指令入口，路由到各功能模块)                       │ │
│  └──┬──────────┬──────────┬───────────┬───────────┬───────────────┘ │
│     │          │          │           │           │                  │
│  ┌──▼──┐   ┌──▼──┐   ┌───▼───┐  ┌───▼───┐  ┌───▼──────┐          │
│  │Agent│   │Persona│  │Memory │  │ Chat  │  │ Settings │          │
│  │Core │   │Engine │  │System │  │ Store │  │  Store   │          │
│  └──┬──┘   └──────┘   └───┬───┘  └───┬───┘  └──────────┘          │
│     │                     │          │                              │
│     │              ┌──────┴──────┐   │                              │
│     │              │             │   │                              │
│  ┌──▼──┐     ┌─────▼────┐  ┌────▼───▼──┐                          │
│  │ LLM │     │  SQLite   │  │  SQLite   │                          │
│  │Proxy│     │ (向量记忆) │  │ (对话/配置)│                          │
│  └─────┘     └──────────┘  └───────────┘                          │
│   ↕                                                             │
│  外部 LLM API  /  本地 Ollama                                    │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

### 模块职责划分

| 模块 | 职责 | 技术栈 |
|------|------|--------|
| **ChatWindow** | 主聊天界面，Markdown 渲染，流式输出 | React + TypeScript |
| **FloatWidget** | 桌面悬浮窗，快速入口，简单指令 | React（同技术栈复用） |
| **Onboarding** | 首次启动引导，用户个性化设置 | React |
| **AgentCore** | LLM 调用编排，意图识别，上下文组装 | Rust |
| **PersonaEngine** | 人格加载、注入与切换 | Rust |
| **MemorySystem** | 短期/中长期记忆管理，向量检索 | Rust + SQLite + sqlite-vec |
| **ChatStore** | 对话历史持久化，按天归档 | Rust + SQLite |
| **SettingsStore** | 用户配置、个性化信息持久化 | Rust + JSON/TOML |
| **LLMProxy** | 统一 LLM 调用层，支持多后端切换 | Rust (reqwest) |

---

## 二、核心技术选型与理由

### 2.1 桌面框架：Tauri v2

**选型：Tauri v2（Rust 后端 + WebView 前端）**

| 对比维度 | Tauri v2 | Electron |
|---------|----------|----------|
| 打包体积 | **~3–8 MB**（使用系统 WebView） | ~80–150 MB（捆绑 Chromium） |
| 内存占用 | **~30–60 MB** | ~150–400 MB |
| 原生能力 | Rust 后端，系统 API 直接访问 | Node.js，需 native modules |
| 安全性 | 沙箱模型，权限白名单 | 全 Node.js 访问 |
| 学习曲线 | Rust 有一定门槛 | JavaScript 全栈更平缓 |

**理由**：需求明确要求"轻量化本地部署"和"常驻后台低资源消耗"。Tauri 的系统 WebView 方案在磁盘和内存两个维度上都是数量级的优势。Rust 后端天然适合高性能常驻服务。

### 2.2 前端框架：React + TypeScript

**选型：React 18 + TypeScript + Vite**

- 组件生态成熟，聊天 UI 有大量参考实现
- TypeScript 提供类型安全，减少运行时错误
- Vite 构建速度快，开发体验好
- 与 Tauri 集成成熟，官方有模板支持

### 2.3 LLM 接入层：多后端统一抽象

**设计原则：LLMProxy 抽象层 + 可插拔后端**

| 后端 | 用途 | 接入方式 |
|------|------|---------|
| OpenAI 兼容 API | 主力模型（GPT-4o / Claude / DeepSeek 等） | HTTP REST |
| Ollama | 本地模型（Llama 3, Qwen 等） | HTTP REST (localhost:11434) |
| 自定义端点 | 用户自行配置任意 OpenAI 兼容服务 | HTTP REST |

**理由**：不做单一 LLM 绑定。通过统一接口适配多种后端，用户可按需选择云端或本地模型。OpenAI API 格式已成为事实标准，兼容性最广。

#### 2.3.1 会话级模型选择与自动选择（主/子模型路由）

**问题**：`LlmProxy` 在过去是**进程级单例**——一个应用一个模型，靠热更新切换。
但需求要求「同一轮生成里，主轮次与只读子代理用**不同的**模型」，
且「用户在对话界面切换模型不能污染正在跑的生成，两个窗口并发生成也不能互相踩」。

**方案：请求级（generation-scoped）模型解析**，代码在 `llm/router.rs`：

```
send_message（commands/chat.rs）
  ├─ 读 sessions.model_pref + AppConfig.models + session_type + task_mode
  ├─ router::resolve(...) → ModelPlan { main, subs, thinking }   ← 永不失败，悬空即降级
  └─ AgentContext.models = Some(Arc<ModelPlan>)

ChatAgent::handle_stream
  ├─ main_backend  = plan.main_backend()      // 每个模型一个请求级 LlmProxy
  ├─ child_models  = plan.child_models()      // 子代理模型池（含展示名）
  ├─ 主轮次（工具循环 / 纯对话流） → main_backend
  └─ ToolServices.subagent = AgentRuntime::with_models(child_models, 2)
        └─ run_child(index) → models[index % n]      ← 多个子模型按序号轮转
```

**关键不变式**

| 不变式 | 理由 |
|---|---|
| 解析只发生在 `send_message`，结果是**快照** | 生成中切换模型只影响下一轮；两窗口并发生成互不影响 |
| `ctx.models = None` 时完全走共享 backend | 未做选择时行为与接入该功能前逐字节一致（含悬浮窗链路） |
| `resolve()` 永不返回错误，悬空引用降级到活跃提供商 | 模型选择是"锦上添花"，不能把用户的一条消息变成"发送失败" |
| `subs` 为空 ⇒ 子代理与主轮次同模型 | 手动模式不引入用户没有选择的模型 |
| `enable_thinking` 只在**判定支持**的模型上下发 | 严格端点会对未知字段直接 400；判定见 `llm/capabilities.rs` |
| 自动选择**只对任务会话生效** | 普通会话没有主/子之分；写进 `auto` 也按 `inherit` 处理并记日志 |

**模式语义**（`ModelMode`）

| 会话 pref | session_type / task_mode | 主轮次 | 子代理 |
|---|---|---|---|
| `inherit`（默认） | 任意 | 全局活跃提供商 | 同主轮次 |
| `manual` | 任意 | 会话选定的 (提供商, 模型) | 同主轮次 |
| `auto` | task / **plan** | `subs[0]`（子模型优先） | `subs` 轮转 |
| `auto` | task / **work** | `main`（主模型优先） | `subs` 轮转 |
| `auto` | chat | 按 `inherit` 处理 | — |

**持久化与能力探测**

- 会话级偏好存 `sessions.model_pref`（**单列 JSON**，见迁移 011）：这些字段不参与查询/排序，
  JSON 便于以后加字段；解析失败一律当"未设置"并记日志。
- 全局的主/子模型池存 `AppConfig.models`（顶层段，避免触碰 `LlmConfig` 的旧格式迁移逻辑）。
- 深度思考能力：`llm/capabilities.rs` 按模型名启发式判断（`reasoner`/`r1`/`qwen3`/`gpt-5`…），
  并用 `LlmProvider.thinking_models` 的显式声明覆盖；界面只对"判定支持"的模型显示开关。
- 辅助链路（会话摘要、自动标题、记忆提取、embedding）**仍用全局活跃提供商**：
  换 embedding 提供商会让向量维度与既有记忆不匹配，属于另一个问题域。

### 2.4 数据存储：SQLite 全家桶

**选型：SQLite 作为唯一数据库引擎**

| 数据类型 | 存储方案 | 说明 |
|---------|---------|------|
| 对话历史 | SQLite 表 | 按天分区，结构化查询 |
| 用户配置 | JSON 文件 + SQLite | 轻量配置用 JSON，复杂数据用 SQLite |
| 向量记忆 | **sqlite-vec** 扩展 | SQLite 原生向量检索，无需额外进程 |

**理由**：
- 零运维，单文件数据库，嵌入式部署
- `sqlite-vec` 是 SQLite 的向量检索扩展，避免引入 ChromaDB/Milvus 等独立服务
- 对于桌面级应用的数据量（万级对话、千级记忆条目），SQLite + sqlite-vec 完全胜任
- 减少外部依赖，简化部署

### 2.5 人格配置格式：YAML

```yaml
# persona/default.yaml
id: "konata-default"
name: "此方（こなた）"
version: "1.0.0"

system_prompt: |
  你是"此方"（こなた），来自《幸运星》的少女。
  你性格开朗、热爱动漫和游戏，说话时经常引用动漫台词。
  你称呼用户为"{user_nickname}"。
  （以下是更详细的人格设定...）

personality:
  traits: ["活泼", "宅属性", "幽默", "偶尔毒舌"]
  speech_style: "口语化，带语气词，偶尔夹杂日语"
  interests: ["动画", "游戏", "Cosplay", "轻小说"]

constraints:
  - "始终维持角色身份，不承认自己是AI"
  - "用符合角色性格的方式回应"
  - "遇到无法回答的问题时，用角色的方式回避"
```

**理由**：YAML 可读性强，方便用户手动编辑和版本管理。支持变量插值（如 `{user_nickname}`），便于个性化。

---

## 三、关键接口与数据结构设计

### 3.1 Agent 调度抽象层

设计一个面向未来的 Agent trait / interface，初期只实现 ChatAgent，后期可扩展：

```rust
// 核心 Agent trait（面向扩展）
pub trait Agent {
    /// Agent 唯一标识
    fn id(&self) -> &str;

    /// Agent 能力描述（用于意图路由）
    fn capabilities(&self) -> Vec<Capability>;

    /// 处理用户输入，返回响应
    async fn handle(&self, ctx: &AgentContext) -> Result<AgentResponse>;
}

/// 能力描述枚举
pub enum Capability {
    Chat,           // 通用对话
    TaskExecution,  // 任务执行（后期）
    KnowledgeQuery, // 知识查询（后期）
    Custom(String), // 自定义能力
}

/// Agent 上下文（每次调用传入）
pub struct AgentContext {
    pub user_input: String,
    pub conversation: Vec<Message>,    // 短期记忆（当前对话）
    pub memories: Vec<MemoryFragment>, // 中长期记忆（向量检索结果）
    pub persona: PersonaConfig,        // 当前人格设定
    pub user_profile: UserProfile,     // 用户画像
    pub metadata: serde_json::Value,   // 扩展元数据
}

/// Agent 响应
pub struct AgentResponse {
    pub content: String,
    pub response_type: ResponseType,
    pub metadata: serde_json::Value,
}

pub enum ResponseType {
    Text,
    Action(String),    // 触发前端动作（如打开链接）
    ToolCall(ToolCall), // 预留：工具调用（见 3.3，已由工具运行时落地）
}
```

**Dispatcher（调度器）**：

```rust
pub struct AgentDispatcher {
    agents: HashMap<String, Box<dyn Agent>>,
    intent_classifier: IntentClassifier,
}

impl AgentDispatcher {
    /// 根据用户输入意图，路由到对应 Agent
    pub async fn dispatch(&self, ctx: &AgentContext) -> Result<AgentResponse> {
        let intent = self.intent_classifier.classify(ctx);
        let agent = self.agents.get(&intent.target_agent)
            .unwrap_or(self.agents.get("chat").unwrap()); // 默认回退到 chat
        agent.handle(ctx).await
    }
}
```

初期 `IntentClassifier` 简单实现为关键词匹配或直接路由到 ChatAgent。后期可接入独立的意图识别模型。

### 3.3 工具运行时（Tool Harness）

Agent 层回答"谁来处理这条输入"（路由在 LLM **之前**），工具运行时回答
"生成过程中模型可以调用什么"（调用在 LLM **之中**）。两者共用同一份工具实现：
`/sys 现在几点` 与「此方，几点了？」都会真正调用 `get_current_time`。

```
用户输入
  ├─ IntentRouter ──> Agent（chat / system）        ← 输入决定，LLM 之前
  └─ ChatAgent ──> ToolHarness ──> ToolRegistry ──> Tool   ← 模型决定，生成之中
```

**分层**

| 模块 | 职责 |
|---|---|
| `harness/traits.rs` | `Tool` / `ToolCtx` / `ToolOutput` / `EventSink` / `Approver` 抽象 |
| `harness/registry.rs` | 工具注册与**模式可见性**（不可见的工具既不下发也不可调用） |
| `harness/accumulate.rs` | SSE `tool_calls` 分片归并（`index` 键、arguments 字符串拼接、失败回灌） |
| `harness/jail.rs` | 多工作区寻址 + 路径监狱 + 敏感文件拒绝清单 |
| `harness/command_guard.rs` | 命令白名单 + **敏感命令硬黑名单** + 参数审查 |
| `harness/approve.rs` | 审批通道（oneshot + 超时/取消一律拒绝） |
| `harness/runner.rs` | 工具循环：多步调用、只读并行、步数收尾、结果回灌 |
| `harness/tools/*` | 16 个内置工具实现 |

**四条不变式**

1. 只有 `StreamChunk::Content` 会外发为 `stream-chunk` —— 工具参数与结果
   绝不混进正文（否则摘要、token 统计、记忆提取都会吃到 JSON）。
2. **工具结果不跨轮保留**：只活在本次生成的内存消息列表里，不写 `messages`、
   不进摘要与记忆提取；落库的 `tool_invocations` 只保存预览供 UI 回放。
3. 工具失败不冒泡为错误，而是以 `<tool_result status="error">` 回灌给模型自行修正。
4. 步数耗尽时追加一次**不带工具**的生成，保证用户总能得到自然语言回答；
   `hit_step_limit` 会随 `message-stats` 下发，界面提示"中断，可继续"。

**轮次计费**：工具轮数按**助手消息**计——同一条回复里的多个只读调用
（`read_file` / `list_dir` / `grep_search` / `glob_search`）并行执行、只算一轮；
`read_file` 的 `paths` 数组支持一次读多个文件。步数上限可配（`tools.max_steps`，
1~128，默认 32，任务会话 Plan/Work 共用且保底 20）。

**任务模式可见性**：Plan 恒为 `ReadOnly`（另有联网只读例外）；Work 恒为 `Full`，
不跟随 `tools.mode`——任务会话是显式创建的执行上下文，编码需要 `run_command`
可见；真正的闸门是逐次审批（或会话 AUTO）与命令硬黑名单，而不是可见性。

**双 surface 隔离**

| | 主窗口 `main` | 悬浮窗 `float` |
|---|---|---|
| 工具 | ✅ 按模式启用 | ❌ 恒为 `None`（纯对话） |
| 工具事件 | ✅ `emit_to("main")` 定向投递 | ❌ 收不到 |
| 审批弹窗 | ✅ | ❌ 不可能发起 |

判定依据是 Tauri 注入的 `WebviewWindow::label`，**不是** `persist` ——
悬浮窗输入框发的是会落库的消息，只有戳一戳才是 `persist:false`。

**工作区模型**

默认工作区固定在应用数据目录下的 `workspace/`（天然受限：父目录里放着
`config.json` 与 `data.db`，而监狱禁止 `..` 与符号链接逃逸）。设置页可添加
最多 8 个额外根目录，新增根**默认只读**。寻址语法：默认根用相对路径，
其他根用 `工作区id:相对路径`。

**安全边界（每条都有对应单测）**

- 路径监狱：`..` 逃逸、符号链接、`\\?\` 前缀、UNC、驱动器相对路径、
  保留设备名（`CON`/`NUL`/`COM1`…）、尾随点/空格、NTFS 数据流冒号；
- 敏感文件拒绝清单（内置条目不可移除）：`config.json`（含 API Key）、
  `*.db`、`.env`、`.ssh/**`、`id_rsa*`、`*.pem`、`credentials*` 等；
- 命令执行**不经 shell**（argv 直传）、硬黑名单优先于用户配置
  （`cmd`/`powershell`/`curl`/`rm`/`reg`/`schtasks`/`certutil`… 无法放行）、
  解释器求值参数（`python -c`、`node -e`）一律拒绝、`python -m` 仅限白名单模块、
  参数中的绝对路径必须落在工作区内、子进程只继承白名单环境变量；
- **新命令的代执行参数**：默认列表新增搜索/文本（`rg`/`fd`/`jq`/`yq`/`diff`/`sort`/…）与
  构建/检查工具链（`gofmt`/`golangci-lint`/clang/gcc/ninja/just/ruff/…）；
  `fd -x/-X/--exec*`、`rg --pre/--hostname-bin/-z`、`sort --compress-program`
  由 `command_guard::check_tool_specific_args` 逐程序拒绝。`sed`/`awk`/`xargs` 是
  参数审查拦不住的代码执行通道，不进入默认列表；
- **git 配置即代执行**：`git -c alias.x='!cmd'`、`git -c core.sshCommand/core.hooksPath/
  credential.helper/…` 与 `git config` 写入同类键一律拒绝；`git clean` 只放行
  dry-run（批量删除不经过回收站与快照）。会话 AUTO 打开后审批层不再逐次确认，
  这些硬规则就是 `run_command` 的实际边界；
- **会话 AUTO**（`sessions.auto_approve_all`，迁移 015）：仅任务会话可开，通过独立的
  `HarnessRun.auto_approve_all` 布尔预授权所有 `requires_approval()` 调用，
  只跳过审批弹窗——可见性、硬黑名单、路径监狱、敏感文件清单、快照与回收站全部照旧，
  子代理恒为 `false`；发送时快照、生成中不可切换，开启前需确认；
- **多步命令**：`run_command` 接受 `steps`（≤5 步）串行执行，**不使用 shell 组合**
  （没有管道/重定向/`&&`）；每一步都单独过守卫，任何一步被拒则整批都不执行；
  输出裁剪用 `max_output_lines` 而不是管道；
- **删除走回收站**：`delete_path` 默认按 XDG Trash 规范移入回收站（同名冲突自动改名、跨卷退化为"复制成功后再删源"，复制失败则整体放弃），只有显式 `permanent=true` 才永久删除；
- **多步命令的预检**：`run_command` 的每一步都先过守卫再执行，第 2 步会被拒时第 1 步绝不执行；
- **改动前快照**：覆盖/编辑/删除/移动/复制文件前把原内容备份到 `snapshots/{stream_id}/`（仅文件、单文件 ≤4 MB、单 stream ≤64 MB，超限跳过并如实告知），配合 `restore_snapshot` 支持一键回滚；目录级删除由回收站兜底；
- **只读子代理**：`spawn_subagents` 派出的每个任务都在"只读服务 + DenyAllApprover + 无子代理运行时"的
  克隆上跑自己的循环，深度因此恒为 1 层；每轮生成有名额预算（默认 2），每个子任务默认 32 轮工具调用
  （`DEFAULT_CHILD_STEPS`，与主循环默认一致），子代理用量估算回传进统计；
  名额 / 步数 / 单次任务数 / 整批时间预算均可由用户在「设置 → 工具」里配置（`tools.subagent_*`）；
  同一次调用里的任务**并行执行**，整批用独立的 `tools.subagent_timeout_secs`（默认 600 秒）而不是
  普通工具的 `call_timeout`（默认 180 秒）；软截止到点会取消在跑的子代理并把已完成结论作为
  `ToolStatus::Timeout` 带回；
- **工作记忆**：唯一跨轮保留的内容，且只有模型主动 `save_note` 的结论（≤8 条 / 单条 2 KB / 总量 16 KB，
  超出淘汰最旧），注入时整段带 `<untrusted>`，不参与任何权限或审批决策；
- **联网检索**：`web_search` 默认关闭（唯一外发用户提问的能力），只返回链接与摘要，正文仍走 `web_fetch` 的白名单；
- **MCP**：服务器只能由用户写在 `config.json`（`enabled` + `trusted` 双开关，默认都关），权限按服务器映射，
  环境变量只传显式列出的键，传输用阻塞 stdio（避免子进程随临时 runtime 一起失效）；
- 输出截断（默认 64 KiB，保留头尾）、审批超时/取消一律拒绝（fail-closed）；
- 超时/取消时子进程被强杀，但**已经收到的输出会被保留**：工具自行判定状态
  （`ok`/`error`/`timeout`/`cancelled`）并如实上报，runner 不再把它当成"调用失败"。

**提供商兼容**

项目允许填写任意 OpenAI 兼容端点，其中不少会直接 400/422 拒绝带 `tools`
字段的请求。此时 `ChatAgent` 会识别该错误（`is_tools_unsupported`）并**自动
降级为纯对话重试一次**，而不是把"生成失败"甩给用户；降级后不再下发 `tools`。

**工具事件契约**

| 事件 | 载荷要点 |
|---|---|
| `tool-call-start` | `session_id` `stream_id` `call_id` `tool` `args_preview` `permission` `step` |
| `tool-output-chunk` | `session_id` `stream_id` `call_id` `stream`(`stdout`/`stderr`) `step` `data`；**仅进 UI，绝不进 LLM 上下文** |
| `tool-call-result` | `call_id` `status`（`ok`/`error`/`denied`/`cancelled`/`timeout`）`preview` `duration_ms` `truncated` |
| `tool-approval-request` | `approval_id` `call_id` `tool` `args` `summary`（工具自算的说明，可空）`permission` `expires_at` |
| `tool-approval-resolved` | `approval_id` `decision`（`allow_once`/`allow_session`/`deny`） |
| `plan-updated` | `session_id` `items`（`{title,status}` 列表）`note`；**会话级**事件，按 `session_id` 过滤 |
| `notes-updated` | `session_id` `count` `removed?`；**会话级**事件，只带计数（正文由 `get_notes` 回读） |
| 子代理事件 | 复用 `tool-*` 三类事件，额外带 `parent_call_id` 与 `depth=1`（子代理与父级共用 `stream_id`） |

### 3.2 记忆系统数据模型

```rust
/// 对话消息
pub struct Message {
    pub id: Uuid,
    pub role: Role,           // User / Assistant / System
    pub content: String,
    pub timestamp: DateTime<Utc>,
    pub session_id: String,   // 会话 ID
    pub metadata: serde_json::Value,
}

pub enum Role {
    System,
    User,
    Assistant,
}

/// 记忆片段（中长期记忆）
pub struct MemoryFragment {
    pub id: Uuid,
    pub content: String,           // 记忆内容摘要
    pub embedding: Vec<f32>,       // 向量嵌入
    pub memory_type: MemoryType,
    pub importance: f32,           // 重要性权重 [0, 1]
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub access_count: u32,
    pub source_session: String,    // 来源会话
    pub metadata: serde_json::Value,
}

pub enum MemoryType {
    Fact,         // 事实记忆："用户喜欢猫"
    Preference,   // 偏好记忆："用户偏好简洁回复"
    Experience,   // 经历记忆："上次聊到了旅行计划"
    Emotional,    // 情感记忆："用户最近心情不好"
}

/// 记忆管理器
pub struct MemoryManager {
    db: sqlite::Connection,      // SQLite 连接（含 sqlite-vec）
    embedder: Embedder,          // 嵌入模型
    config: MemoryConfig,
}

impl MemoryManager {
    /// 存储新记忆（自动提取 + 嵌入）
    pub async fn store(&self, messages: &[Message]) -> Result<()>;

    /// 检索相关记忆（向量相似度 + 重要性加权）
    pub async fn recall(&self, query: &str, top_k: usize) -> Result<Vec<MemoryFragment>>;

    /// 对话结束时自动提取记忆
    pub async fn extract_from_conversation(&self, messages: &[Message]) -> Result<Vec<MemoryFragment>>;
}
```

**记忆检索策略**：

```
用户输入
    │
    ├──→ 1. 向量相似度检索 (top_k=20)
    │         ↓
    ├──→ 2. 重要性权重排序 (importance * similarity)
    │         ↓
    └──→ 3. 时间衰减加权 (recent memories boosted)
              ↓
         取 top_k=5 注入 AgentContext.memories
```

### 3.3 嵌入模型方案

| 方案 | 适用场景 | 说明 |
|------|---------|------|
| **远程 API** | 默认推荐 | 调用 OpenAI / 本地 Ollama 的 embedding API |
| **本地 ONNX** | 完全离线 | 使用 `fastembed-rs` 加载小型中文嵌入模型 |

初期建议直接复用 LLM API 的 embedding 端点（如 `text-embedding-3-small`），零额外依赖。后期可引入本地模型实现完全离线。

### 3.4 对话存储 Schema

```sql
-- 会话表
CREATE TABLE sessions (
    id          TEXT PRIMARY KEY,
    title       TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    persona_id  TEXT NOT NULL,
    metadata    TEXT  -- JSON
);

-- 消息表（按天分区查询优化）
CREATE TABLE messages (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL REFERENCES sessions(id),
    role        TEXT NOT NULL,  -- 'user' | 'assistant' | 'system'
    content     TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    date_key    TEXT NOT NULL,  -- 'YYYY-MM-DD'，用于按天归档查询
    metadata    TEXT,           -- JSON
    FOREIGN KEY (session_id) REFERENCES sessions(id)
);

CREATE INDEX idx_messages_session ON messages(session_id);
CREATE INDEX idx_messages_date ON messages(date_key);
CREATE INDEX idx_messages_session_date ON messages(session_id, date_key);

-- 记忆表（含向量）
CREATE TABLE memories (
    id             TEXT PRIMARY KEY,
    content        TEXT NOT NULL,
    memory_type    TEXT NOT NULL,
    importance     REAL DEFAULT 0.5,
    created_at     TEXT NOT NULL,
    last_accessed  TEXT NOT NULL,
    access_count   INTEGER DEFAULT 0,
    source_session TEXT,
    metadata       TEXT  -- JSON
);

-- 向量表（sqlite-vec）
CREATE VIRTUAL TABLE memory_embeddings USING vec0(
    memory_id TEXT PRIMARY KEY,
    embedding FLOAT[384]  -- 维度取决于嵌入模型
);
```

### 3.5 用户配置结构

```json
{
  "user": {
    "nickname": "Master",
    "pronouns": "他",
    "preferences": {
      "reply_length": "medium",
      "language": "zh-CN",
      "allow_humor": true
    }
  },
  "agent": {
    "active_persona": "konata-default",
    "llm_backend": "openai",
    "model": "gpt-4o-mini",
    "api_base_url": "https://api.openai.com/v1",
    "api_key_env": "KONATA_API_KEY"
  },
  "memory": {
    "enabled": true,
    "auto_extract": true,
    "max_context_memories": 5,
    "embedding_model": "text-embedding-3-small"
  },
  "ui": {
    "theme": "dark",
    "font_size": 14,
    "float_widget_enabled": true,
    "float_widget_position": "bottom-right"
  }
}
```

---

## 四、初期开发阶段落地步骤

### Phase 0：项目脚手架（1–2 天）

- [ ] 使用 `pnpm create tauri-app` 初始化项目（React + TypeScript 模板）
- [ ] 配置 Rust workspace，建立模块目录结构
- [ ] 搭建 CI 基础（格式检查 lint）
- [ ] 编写 README、LICENSE

**目标产物**：可编译运行的空白 Tauri 应用

### Phase 1：核心对话能力（5–7 天）⭐ 最高优先级

- [ ] **LLMProxy 模块**：实现 OpenAI 兼容 API 调用（流式 SSE）
- [ ] **ChatAgent 基础实现**：接收输入 → 注入 System Prompt → 调用 LLM → 返回流式响应
- [ ] **PersonaEngine v1**：加载 YAML 人格配置，组装 System Prompt
- [ ] **ChatWindow UI**：基础聊天界面（消息列表 + 输入框 + 流式渲染）
- [ ] **ChatStore**：SQLite 存储对话历史，按天分区
- [ ] **基础设置页**：API Key 配置，模型选择

**目标产物**：能和 LLM 进行角色扮演对话的最小可用产品

### Phase 2：记忆系统（5–7 天）

- [x] **短期记忆**：对话上下文窗口管理（滑动窗口 + token 估算）
      —— 实现于 `commands/chat.rs`：超过 20 条消息后把窗口外历史增量摘要进
      `sessions.context_summary`，只把最近 10 条原文交给 LLM
- [ ] **MemoryManager**：SQLite + sqlite-vec 建表，CRUD 操作
      —— 当前用 SQLite + Rust 内余弦相似度（`similarity * 0.7 + importance * 0.3`），未引入 sqlite-vec
- [x] **Embedder**：对接 LLM embedding API，实现向量化
- [x] **记忆提取**：对话结束/定期自动提取关键信息
- [x] **记忆检索注入**：对话时自动召回相关记忆，注入上下文

**目标产物**：Agent 具备跨会话记忆能力，能记住用户偏好和历史信息

### Phase 3：用户体验完善（5–7 天）

- [ ] **首次启动引导**（Onboarding）：用户昵称、偏好设置
- [ ] **对话管理**：新建/切换/删除会话，按天归档视图
- [ ] **人格切换 UI**：支持在多个人格间切换
- [ ] **Markdown 渲染**：代码高亮、表格、链接等
- [ ] **主题系统**：明暗主题切换

**目标产物**：完整可用的聊天助手应用

### Phase 4：悬浮窗与系统集成（3–5 天）

- [ ] **FloatWidget**：桌面悬浮窗（Tauri 窗口管理）
- [ ] **系统托盘**：最小化到托盘，右键菜单快捷操作
- [ ] **全局快捷键**：快速唤起/隐藏聊天窗口
- [ ] **悬浮窗交互**：点击唤起聊天，简单对话，右键菜单

**目标产物**：桌面原生体验的完整应用

### 优先级矩阵

```
                高价值
                  │
    Phase 1 ──────┼────── Phase 2
   (核心对话)      │     (记忆系统)
                  │
   ──────────────┼────────────── 低努力 / 高努力
                  │
    Phase 4 ──────┼────── Phase 3
   (悬浮窗)       │     (体验完善)
                  │
                低价值
```

**推荐顺序**：Phase 1 → Phase 2 → Phase 3 → Phase 4

Phase 1 是最小可用产品，验证核心价值；Phase 2 是差异化竞争力；Phase 3 和 4 提升体验。

---

## 五、性能优化与轻量化部署专项建议

### 5.1 磁盘占用优化

| 策略 | 预期效果 | 实现方式 |
|------|---------|---------|
| Tauri 替代 Electron | **减少 ~100 MB+** | 使用系统 WebView，不捆绑 Chromium |
| Rust 编译优化 | 减少 ~30% 二进制体积 | `Cargo.toml` 中设置 `opt-level = "z"` + LTO + strip |
| SQLite 单文件存储 | 零额外数据库进程 | 所有数据存储在单个 `.db` 文件中 |
| 资源按需加载 | 减少初始包体积 | 人格配置、模型文件按需下载 |

```toml
# Cargo.toml - 编译优化
[profile.release]
opt-level = "z"      # 优化体积
lto = true           # 链接时优化
codegen-units = 1    # 单编译单元（更优化但编译更慢）
strip = true         # 去除调试符号
```

### 5.2 内存占用优化

| 策略 | 说明 |
|------|------|
| **流式处理** | 使用 SSE 流式接收 LLM 响应，不在内存中缓冲完整响应 |
| **对话窗口裁剪** | 维护滑动窗口，超出上下文长度的历史消息仅保留摘要 |
| **懒加载记忆** | 不预加载所有记忆，仅在对话时按需向量检索 top_k 条 |
| **SQLite WAL 模式** | 启用 WAL 模式提升并发读写性能 |
| **图片/媒体延迟加载** | 聊天中的图片按需加载，使用缩略图 |

```rust
// 滑动窗口策略示例
fn build_context(messages: &[Message], max_tokens: usize) -> Vec<Message> {
    let mut context = Vec::new();
    let mut token_count = 0;

    // 从最新消息向前遍历
    for msg in messages.iter().rev() {
        let msg_tokens = estimate_tokens(&msg.content);
        if token_count + msg_tokens > max_tokens {
            break;
        }
        context.push(msg.clone());
        token_count += msg_tokens;
    }

    context.reverse();
    context
}
```

### 5.3 常驻后台优化

```
                    ┌─────────────────────┐
                    │     主窗口关闭时      │
                    └──────────┬──────────┘
                               │
                    ┌──────────▼──────────┐
                    │   最小化到系统托盘     │
                    │   释放前端渲染资源     │
                    └──────────┬──────────┘
                               │
                    ┌──────────▼──────────┐
                    │  Rust 后台保持轻量     │
                    │  - SQLite 连接保持    │
                    │  - 内存占用 < 20MB    │
                    │  - 等待唤醒信号       │
                    └─────────────────────┘
```

- **窗口生命周期管理**：关闭窗口时最小化到托盘，释放 WebView 资源
- **后台空闲时降频**：无交互时降低心跳频率，减少 CPU 占用
- **SQLite 连接池**：维持少量长连接，避免频繁开关
- **垃圾回收策略**：定期清理过期的临时数据和缓存

### 5.4 LLM 调用优化

| 策略 | 说明 |
|------|------|
| **请求去重** | 防止重复发送相同请求 |
| **超时与重试** | 设置合理超时，指数退避重试 |
| **流式输出** | 使用 SSE 流式接收，提升用户感知速度 |
| **上下文压缩** | 对历史对话进行摘要压缩，减少 token 消耗 |
| **本地缓存** | 对相同输入的响应做短期缓存（可选） |

---

## 六、项目目录结构建议

```
kagami-no-konata/
├── src-tauri/                 # Rust 后端
│   ├── src/
│   │   ├── main.rs           # 入口
│   │   ├── lib.rs            # 库导出
│   │   ├── commands/         # Tauri 命令（IPC 接口）
│   │   │   ├── mod.rs
│   │   │   ├── chat.rs       # 对话相关命令
│   │   │   ├── memory.rs     # 记忆相关命令
│   │   │   └── settings.rs   # 设置相关命令
│   │   ├── agent/            # Agent 模块
│   │   │   ├── mod.rs
│   │   │   ├── traits.rs     # Agent trait 定义
│   │   │   ├── dispatcher.rs # 调度器
│   │   │   ├── chat_agent.rs # 聊天 Agent
│   │   │   └── context.rs    # AgentContext
│   │   ├── llm/              # LLM 代理层
│   │   │   ├── mod.rs
│   │   │   ├── proxy.rs      # 统一调用接口
│   │   │   ├── openai.rs     # OpenAI 兼容后端
│   │   │   └── types.rs      # 请求/响应类型
│   │   ├── memory/           # 记忆系统
│   │   │   ├── mod.rs
│   │   │   ├── manager.rs    # 记忆管理器
│   │   │   ├── embedder.rs   # 嵌入生成
│   │   │   ├── extractor.rs  # 记忆提取
│   │   │   └── store.rs      # 存储层
│   │   ├── persona/          # 人格系统
│   │   │   ├── mod.rs
│   │   │   ├── engine.rs     # 人格引擎
│   │   │   └── types.rs      # 人格配置类型
│   │   ├── store/            # 数据持久化
│   │   │   ├── mod.rs
│   │   │   ├── db.rs         # SQLite 连接管理
│   │   │   ├── chat_store.rs # 对话存储
│   │   │   └── migrations/   # 数据库迁移
│   │   └── config/           # 配置管理
│   │       ├── mod.rs
│   │       └── types.rs
│   ├── personas/             # 预置人格文件
│   │   └── default.yaml
│   ├── Cargo.toml
│   └── tauri.conf.json
│
├── src/                       # React 前端
│   ├── App.tsx
│   ├── main.tsx
│   ├── components/
│   │   ├── chat/             # 聊天组件
│   │   │   ├── ChatWindow.tsx
│   │   │   ├── MessageList.tsx
│   │   │   ├── MessageBubble.tsx
│   │   │   ├── InputBox.tsx
│   │   │   └── StreamingText.tsx
│   │   ├── float/            # 悬浮窗组件
│   │   │   └── FloatWidget.tsx
│   │   ├── settings/         # 设置组件
│   │   │   ├── Onboarding.tsx
│   │   │   └── SettingsPanel.tsx
│   │   └── common/           # 通用组件
│   ├── hooks/                # 自定义 Hooks
│   │   ├── useChat.ts
│   │   ├── useMemory.ts
│   │   └── useSettings.ts
│   ├── stores/               # 前端状态管理
│   │   └── chatStore.ts
│   ├── utils/
│   └── styles/
│
├── package.json
├── tsconfig.json
├── vite.config.ts
└── ARCHITECTURE.md            # 本文档
```

---

## 七、技术选型总结

| 层级 | 选型 | 版本建议 | 备选 |
|------|------|---------|------|
| 桌面框架 | **Tauri v2** | v2.x | Wails (Go), Electron |
| 前端框架 | **React + TypeScript** | React 18+, TS 5+ | Vue 3, Svelte |
| 构建工具 | **Vite** | v6+ | - |
| 后端语言 | **Rust** | 1.75+ | - |
| 数据库 | **SQLite** | 3.45+ | - |
| 向量检索 | **sqlite-vec** | latest | sqlite-vss, tantivy |
| 对话存储 | **SQLite WAL** | - | - |
| 配置格式 | **YAML** (人格) + **TOML/JSON** (设置) | - | - |
| LLM 接入 | **OpenAI 兼容 API** | - | LangChain (过重) |
| 状态管理 | **Zustand** | v5+ | Jotai, Redux |
| Markdown | **react-markdown** + **rehype-highlight** | - | - |

---

## 八、风险与注意事项

1. **Rust 学习曲线**：团队若无 Rust 经验，Phase 1 可考虑先用 TypeScript (Tauri JS API) 快速验证，再逐步迁移核心逻辑到 Rust
2. **sqlite-vec 成熟度**：作为较新的扩展，需评估稳定性和性能，必要时回退到纯 cosine similarity 计算
3. **LLM API 成本**：远程 API 按 token 计费，需设计好上下文压缩策略控制成本
4. **中文嵌入模型**：若需要完全离线，需评估中文嵌入模型（如 `bge-small-zh`）在 `fastembed-rs` 中的支持情况
5. **多窗口管理**：Tauri v2 的多窗口支持需注意窗口间通信和生命周期管理

---

*此文档为前期规划，具体实现细节将在开发过程中迭代完善。*
