import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useChatStore } from "../../stores/chatStore";
import {
  TOOL_PERMISSION_LABEL,
  normalizePermission,
  type ToolInfo,
  type WorkspaceView,
} from "../../types/tools";
import {
  modelRefValue,
  parseModelRefValue,
  type ModelSettings,
} from "../../types/models";

/** 子模型数量上限（与后端 `config::types::MAX_SUB_MODELS` 保持一致） */
const MAX_SUB_MODELS = 8;

/** 工具（tool harness）配置段：与后端 `AppConfig.tools` 一一对应 */
interface ToolsConfig {
  enabled: boolean;
  mode: "read_only" | "standard" | "full";
  /** 工作区根目录由 `list_workspaces` / `add_workspace` 单独维护，这里原样透传 */
  workspaces: unknown[];
  deny_globs: string[];
  auto_approve: string[];
  max_steps: number;
  max_output_bytes: number;
  approval_timeout_secs: number;
  /** 单次工具调用超时（秒）：run_command 会据此在超时前收手并保留部分输出 */
  call_timeout_secs: number;
  /** 工作记忆：模型主动记下的跨轮结论 */
  working_memory: boolean;
  /** 子代理预算：每轮生成最多派出几个只读子代理 */
  subagent_max_children: number;
  /** 子代理预算：每个子代理的最大工具轮数 */
  subagent_steps: number;
  /** 子代理预算：单次 spawn_subagents 最多提交几个任务 */
  subagent_max_tasks: number;
  /** 子代理预算：整批子代理的时间预算（秒） */
  subagent_timeout_secs: number;
  /** 联网检索（web_search） */
  search: SearchConfig;
  /** MCP 服务器（外部工具生态） */
  mcp: McpConfig;
  command_allowlist: string[];
  web_domain_allowlist: string[];
}

/** 后端暂时没有返回 tools 段时的兜底默认值（保存时会被原样回写） */
const DEFAULT_TOOLS_CONFIG: ToolsConfig = {
  enabled: true,
  mode: "standard",
  workspaces: [],
  deny_globs: [],
  auto_approve: [],
  max_steps: 32,
  max_output_bytes: 65536,
  approval_timeout_secs: 120,
  call_timeout_secs: 180,
  working_memory: true,
  subagent_max_children: 2,
  subagent_steps: 32,
  subagent_max_tasks: 3,
  subagent_timeout_secs: 600,
  search: {
    enabled: false,
    provider: "searxng",
    endpoint: "",
    api_key: "",
    max_results: 5,
  },
  mcp: { servers: [] },
  command_allowlist: [],
  web_domain_allowlist: [],
};

const TOOL_MODE_HINT: Record<ToolsConfig["mode"], string> = {
  read_only: "只读：写入与执行类工具直接从工具表中移除",
  standard: "标准：读与App内写入自动放行，写文件/执行命令需要审批",
  full: "完整：在标准之上放开命令执行（仍禁止敏感命令）",
};

/** 任务会话的可见性不受上面的权限模式影响：Plan 只读、Work 恒为完整 */
const TASK_MODE_HINT =
  "任务会话例外：Plan 恒为只读，Work 恒为完整（命令仍需审批，除非会话内开启 AUTO）";

type SearchProvider = "searxng" | "tavily" | "brave";

interface SearchConfig {
  enabled: boolean;
  provider: SearchProvider;
  /** 自建实例地址（SearXNG 必填）；官方服务留空用默认地址 */
  endpoint: string;
  api_key: string;
  max_results: number;
}

interface McpServerConfig {
  id: string;
  enabled: boolean;
  trusted: boolean;
  command: string;
  args: string[];
  permission: "read" | "write" | "execute";
}

interface McpConfig {
  servers: McpServerConfig[];
}

interface McpServerStatus {
  id: string;
  connected: boolean;
  tools: string[];
  error?: string | null;
}

const SEARCH_PROVIDER_HINT: Record<SearchProvider, string> = {
  searxng: "自建 SearXNG：填你自己的实例地址（数据不出你的机器，推荐）",
  tavily: "Tavily：留空端点用官方地址，必须填 API Key",
  brave: "Brave Search：留空端点用官方地址，必须填 API Key",
};

const MCP_PERMISSION_HINT: Record<McpServerConfig["permission"], string> = {
  read: "只读：调用不弹审批（只给确实只读的服务器用）",
  write: "写入：每次调用都要审批（默认，最安全）",
  execute: "执行：审批 + 只在「完整」模式下可见",
};

interface LlmProvider {
  id: string;
  name: string;
  api_base_url: string;
  api_key: string;
  model: string;
  enabled_models: string[];
  embedding_model: string;
  /** 留空（null / 缺省）= 不指定，请求体不带 max_tokens，由服务商决定输出上限 */
  max_tokens?: number | null;
  temperature: number;
  enable_thinking?: boolean;
  /** 显式声明支持深度思考的模型（覆盖名称启发式探测） */
  thinking_models?: string[];
}

interface AppConfig {
  user: { nickname: string; pronouns: string; gender: string; birthday: string; bio: string };
  llm: {
    providers: LlmProvider[];
    active_provider_id: string;
  };
  memory: {
    enabled: boolean;
    auto_extract: boolean;
    max_context_memories: number;
  };
  ui: { theme: string; font_size: number; show_message_stats?: boolean; close_action?: string; show_float_clock?: boolean; float_position?: string; poke_enabled?: boolean; poke_probability?: number; poke_llm_chance?: number; bubble_auto_hide_secs?: number };
  /** 工具配置：旧版本后端可能没有该字段，读取时兜底 */
  tools?: ToolsConfig;
  /** 模型路由（自动选择）：旧版本后端可能没有该字段，读取时兜底 */
  models?: ModelSettings;
}

interface UsageStats {
  total_requests: number;
  total_tokens: number;
  prompt_tokens: number;
  completion_tokens: number;
  total_time_ms: number;
}

interface MemoryEntry {
  id: string;
  content: string;
  memory_type: string;
  importance: number;
  source_session: string;
  created_at: string;
  access_count: number;
}

interface ModelInfo {
  id: string;
  object: string;
  owned_by: string | null;
}

/** 导入结果（后端区分成功/跳过/失败，不再把尝试数当成功数） */
interface ImportReport {
  imported: number;
  skipped: number;
  failed: number;
  blocked: number;
  without_embedding: number;
}

function describeReport(report: ImportReport, unit: string): string {
  const parts = [`已导入 ${report.imported} ${unit}`];
  if (report.skipped > 0) parts.push(`跳过 ${report.skipped}`);
  if (report.failed > 0) parts.push(`失败 ${report.failed}`);
  if (report.blocked > 0) parts.push(`安全策略拒绝 ${report.blocked}`);
  if (report.without_embedding > 0) {
    parts.push(`${report.without_embedding} 条缺少向量（换过 embedding 模型时需重新提取）`);
  }
  return parts.join("，");
}

/**
 * 数值输入解析：空值/非法输入回退默认值，并把结果夹取到合法区间
 *
 * 直接 `Number(e.target.value)` 在清空输入框时会得到 0、输入非法字符会得到 NaN，
 * 而 NaN 序列化成 null 会让后端整个配置反序列化失败（表现为"保存失败"但看不出原因）。
 */
function clampNumber(raw: string, min: number, max: number, fallback: number): number {
  if (raw.trim() === "") return fallback;
  const value = Number(raw);
  if (!Number.isFinite(value)) return fallback;
  return Math.min(max, Math.max(min, value));
}

/**
 * 可空数值解析：留空 = `null`（不指定 / 使用服务商默认），非法输入同样回退 `null`
 *
 * 用于 max_tokens 这类"留空才有意义"的字段：后端 `Option<u32>` 收到 null 时
 * 请求体不带该字段。若沿用 `clampNumber` 会在清空输入框时写回 2048，
 * 用户就永远无法表达"不指定"。
 */
function parseOptionalClamped(raw: string, min: number, max: number): number | null {
  if (raw.trim() === "") return null;
  const value = Number(raw);
  if (!Number.isFinite(value)) return null;
  return Math.min(max, Math.max(min, value));
}

export function SettingsPage() {
  const [config, setConfig] = useState<AppConfig | null>(null);
  const [memories, setMemories] = useState<MemoryEntry[]>([]);
  const [stats, setStats] = useState<UsageStats | null>(null);
  const [testResult, setTestResult] = useState<string>("");
  const [testing, setTesting] = useState(false);
  const [saving, setSaving] = useState(false);
  const [fetching, setFetching] = useState(false);

  // 提供商编辑状态
  const [editingProvider, setEditingProvider] = useState<LlmProvider | null>(null);
  const [providerModels, setProviderModels] = useState<ModelInfo[]>([]);
  const [showAddModal, setShowAddModal] = useState(false);
  const [newProviderName, setNewProviderName] = useState("");
  const [newProviderUrl, setNewProviderUrl] = useState("");
  const [newProviderKey, setNewProviderKey] = useState("");

  // 工具状态
  const [tools, setTools] = useState<ToolInfo[]>([]);
  /** MCP 服务器测试连接结果（按 id 索引） */
  const [mcpStatus, setMcpStatus] = useState<Record<string, McpServerStatus>>({});
  const [mcpTesting, setMcpTesting] = useState<string | null>(null);
  const [workspaces, setWorkspaces] = useState<WorkspaceView[]>([]);
  const [workspaceError, setWorkspaceError] = useState("");
  const [newWorkspacePath, setNewWorkspacePath] = useState("");
  const [newWorkspaceLabel, setNewWorkspaceLabel] = useState("");
  const [newWorkspaceWritable, setNewWorkspaceWritable] = useState(false);
  const [addingWorkspace, setAddingWorkspace] = useState(false);

  const setCurrentPage = useChatStore((s) => s.setCurrentPage);

  // 未保存修改检测：user / memory / ui / tools 四段只有点"保存全局配置"才会落盘，
  // 提供商相关的改动则由各自命令即时保存，因此分开跟踪。
  const configSnapshotRef = useRef<string>("");
  const providerSnapshotRef = useRef<string>("");
  const sections = (c: AppConfig) =>
    JSON.stringify({ user: c.user, memory: c.memory, ui: c.ui, tools: c.tools, models: c.models });

  useEffect(() => {
    invoke<AppConfig>("get_config").then((c) => {
      setConfig(c);
      configSnapshotRef.current = sections(c);
      // 默认选中活跃提供商
      const active = c.llm.providers.find((p) => p.id === c.llm.active_provider_id);
      if (active) setEditingProvider({ ...active });
      providerSnapshotRef.current = active ? JSON.stringify(active) : "";
    }).catch((e) => {
      console.error("Failed to load config:", e);
      setTestResult(`❌ 配置加载失败: ${e}`);
    });
    loadMemories();
    invoke<UsageStats>("get_usage_stats").then(setStats).catch(console.error);
    invoke<ToolInfo[]>("list_tools")
      .then(setTools)
      .catch((e) => console.error("Failed to load tools:", e));
    void loadWorkspaces();
  }, []);

  const configDirty =
    config !== null && sections(config) !== configSnapshotRef.current;
  const providerDirty =
    editingProvider !== null &&
    JSON.stringify(editingProvider) !== providerSnapshotRef.current;
  const hasUnsavedChanges = configDirty || providerDirty;

  const handleBack = () => {
    if (hasUnsavedChanges && !confirm("有尚未保存的修改，确认离开设置页？")) return;
    setCurrentPage("chat");
  };

  const loadMemories = async () => {
    try {
      const list = await invoke<MemoryEntry[]>("list_memories", { limit: 50 });
      setMemories(list);
    } catch (e) {
      console.error("Failed to load memories:", e);
    }
  };

  const handleDeleteMemory = async (id: string) => {
    try {
      await invoke("delete_memory", { id });
      setMemories((prev) => prev.filter((m) => m.id !== id));
    } catch (e) {
      setTestResult(`❌ 删除记忆失败: ${e}`);
    }
  };

  const handleClearMemories = async () => {
    if (!confirm("确认清空所有记忆？此操作不可撤销。")) return;
    try {
      await invoke("clear_memories");
      setMemories([]);
      setTestResult("✅ 已清空所有记忆");
    } catch (e) {
      setTestResult(`❌ 清空失败: ${e}`);
    }
  };

  // ─── 提供商管理 ─────────────────────────

  const handleAddProvider = async () => {
    if (!newProviderName || !newProviderUrl) return;
    try {
      const id = await invoke<string>("add_provider", {
        name: newProviderName,
        apiBaseUrl: newProviderUrl,
        apiKey: newProviderKey,
      });
      setNewProviderName("");
      setNewProviderUrl("");
      setNewProviderKey("");
      // 重新加载配置
      const c = await invoke<AppConfig>("get_config");
      setConfig((prev) => (prev ? { ...prev, llm: c.llm } : c));
      const added = c.llm.providers.find((p) => p.id === id);
      if (added) setEditingProvider({ ...added });
      providerSnapshotRef.current = added ? JSON.stringify(added) : "";
      setShowAddModal(false);
      setTestResult("✅ 提供商已添加");
    } catch (e) {
      setTestResult(`❌ 添加失败: ${e}`);
    }
  };

  const handleDeleteProvider = async (id: string) => {
    // 至少保留一个提供商：providers 为空会让整个应用失去 LLM 配置来源
    if ((config?.llm.providers.length ?? 0) <= 1) {
      setTestResult("❌ 至少需要保留一个提供商，无法删除最后一个");
      return;
    }
    if (!confirm("确认删除此提供商？")) return;
    try {
      await invoke("delete_provider", { providerId: id });
      const c = await invoke<AppConfig>("get_config");
      // 只合并 llm 段，避免覆盖用户在其他分区尚未保存的编辑
      setConfig((prev) => (prev ? { ...prev, llm: c.llm } : c));
      const active = c.llm.providers.find((p) => p.id === c.llm.active_provider_id);
      setEditingProvider(active ? { ...active } : null);
      providerSnapshotRef.current = active ? JSON.stringify(active) : "";
      setProviderModels([]);
      setTestResult("✅ 已删除");
    } catch (e) {
      setTestResult(`❌ 删除失败: ${e}`);
    }
  };

  const handleSetActiveProvider = async (id: string) => {
    try {
      await invoke("set_active_provider", { providerId: id });
      const c = await invoke<AppConfig>("get_config");
      setConfig((prev) => (prev ? { ...prev, llm: c.llm } : c));
      const active = c.llm.providers.find((p) => p.id === c.llm.active_provider_id);
      setEditingProvider(active ? { ...active } : null);
      providerSnapshotRef.current = active ? JSON.stringify(active) : "";
      setTestResult("✅ 已切换提供商");
    } catch (e) {
      setTestResult(`❌ 切换失败: ${e}`);
    }
  };

  const handleSaveProvider = async () => {
    if (!editingProvider || !config) return;
    setSaving(true);
    try {
      const payload = { ...editingProvider };
      await invoke("update_provider", {
        providerId: payload.id,
        provider: payload,
      });
      const c = await invoke<AppConfig>("get_config");
      setConfig((prev) => (prev ? { ...prev, llm: c.llm } : c));
      const saved = c.llm.providers.find((p) => p.id === payload.id) ?? payload;
      setEditingProvider({ ...saved });
      providerSnapshotRef.current = JSON.stringify(saved);
      setTestResult("✅ 提供商配置已保存");
    } catch (e) {
      setTestResult(`❌ 保存失败: ${e}`);
    } finally {
      setSaving(false);
    }
  };

  const handleTestProvider = async () => {
    if (!editingProvider) return;
    setTesting(true);
    setTestResult("测试中...");
    try {
      // 先保存再测试
      await invoke("update_provider", {
        providerId: editingProvider.id,
        provider: editingProvider,
      });
      const result = await invoke<string>("test_provider_connection", {
        providerId: editingProvider.id,
      });
      setTestResult(`✅ ${result}`);
    } catch (e) {
      setTestResult(`❌ ${e}`);
    } finally {
      setTesting(false);
    }
  };

  const handleFetchModels = async () => {
    if (!editingProvider) return;
    setFetching(true);
    try {
      // 先保存再获取
      await invoke("update_provider", {
        providerId: editingProvider.id,
        provider: editingProvider,
      });
      const list = await invoke<ModelInfo[]>("fetch_provider_models", {
        providerId: editingProvider.id,
      });
      setProviderModels(list);
    } catch (e) {
      setTestResult(`❌ 获取模型列表失败: ${e}`);
    } finally {
      setFetching(false);
    }
  };

  const handleSetActiveModel = async (modelId: string) => {
    try {
      await invoke("set_active_model", { modelId });
      if (editingProvider) {
        setEditingProvider({ ...editingProvider, model: modelId });
      }
      const c = await invoke<AppConfig>("get_config");
      setConfig(c);
    } catch (e) {
      setTestResult(`❌ 切换模型失败: ${e}`);
    }
  };

  const toggleModelEnabled = (modelId: string) => {
    if (!editingProvider) return;
    const enabled = editingProvider.enabled_models;
    const newEnabled = enabled.includes(modelId)
      ? enabled.filter((m) => m !== modelId)
      : [...enabled, modelId];
    setEditingProvider({ ...editingProvider, enabled_models: newEnabled });
  };

  /** 声明 / 取消声明"该模型支持深度思考"（覆盖名称探测，随"保存"一起落盘） */
  const toggleModelThinking = (modelId: string) => {
    if (!editingProvider) return;
    const current = editingProvider.thinking_models ?? [];
    const next = current.includes(modelId)
      ? current.filter((m) => m !== modelId)
      : [...current, modelId];
    setEditingProvider({ ...editingProvider, thinking_models: next });
  };

  // ─── 模型路由（自动选择） ───────────────────

  /** 全局配置里的模型路由段（旧后端没有该字段时兜底） */
  const modelSettings: ModelSettings = config?.models ?? {
    auto_by_default: false,
    main: null,
    subs: [],
  };

  const updateModelSettings = (patch: Partial<ModelSettings>) => {
    if (!config) return;
    setConfig({ ...config, models: { ...modelSettings, ...patch } });
  };

  /** 目录里所有"已启用"的模型（主/子模型下拉用） */
  const selectableModels: Array<{ value: string; label: string }> = (() => {
    if (!config) return [];
    return config.llm.providers.flatMap((p) => {
      const ids = p.enabled_models.includes(p.model) || !p.model
        ? p.enabled_models
        : [p.model, ...p.enabled_models];
      return ids.map((id) => ({
        value: modelRefValue({ provider_id: p.id, model: id }),
        label: `${id}（${p.name || p.id}）`,
      }));
    });
  })();

  const addSubModel = (value: string) => {
    const ref = parseModelRefValue(value);
    if (!ref) return;
    if (modelSettings.subs.length >= MAX_SUB_MODELS) {
      setTestResult(`❌ 子模型最多 ${MAX_SUB_MODELS} 个`);
      return;
    }
    if (
      modelSettings.subs.some(
        (s) => s.provider_id === ref.provider_id && s.model === ref.model
      )
    ) {
      return;
    }
    updateModelSettings({ subs: [...modelSettings.subs, ref] });
  };

  const removeSubModel = (index: number) => {
    updateModelSettings({ subs: modelSettings.subs.filter((_, i) => i !== index) });
  };

  const moveSubModel = (index: number, delta: number) => {
    const next = [...modelSettings.subs];
    const target = index + delta;
    if (target < 0 || target >= next.length) return;
    [next[index], next[target]] = [next[target], next[index]];
    updateModelSettings({ subs: next });
  };

  // ─── 工具 / 工作区 ─────────────────────────

  const errorText = (e: unknown): string =>
    typeof e === "string" ? e : e instanceof Error ? e.message : String(e);

  const loadWorkspaces = async () => {
    try {
      const list = await invoke<WorkspaceView[]>("list_workspaces");
      setWorkspaces(list);
      setWorkspaceError("");
    } catch (e) {
      console.error("Failed to load workspaces:", e);
      setWorkspaceError(`加载工作区失败：${errorText(e)}`);
    }
  };

  /**
   * 把后端最新的 tools 段同步回本地配置
   *
   * 工作区的增删改由独立命令**立即落盘**，而"保存全局配置"会回写整份 config：
   * 不同步的话，用户加完工作区再点保存，就会用挂载时那份过期的 workspaces
   * 把刚加的根目录覆盖掉。同步时只动 tools 段，并让脏检查快照跟上，
   * 避免把已经落盘的改动误报成"未保存的修改"。
   */
  const syncToolsFromBackend = async () => {
    try {
      const c = await invoke<AppConfig>("get_config");
      setConfig((prev) => (prev ? { ...prev, tools: c.tools ?? prev.tools } : c));
      if (configSnapshotRef.current) {
        const snapshot = JSON.parse(configSnapshotRef.current) as Record<string, unknown>;
        configSnapshotRef.current = JSON.stringify({ ...snapshot, tools: c.tools });
      }
    } catch (e) {
      console.error("Failed to sync tool config:", e);
    }
  };

  /** 修改工具的启用开关 / 权限模式（随"保存全局配置"一起落盘） */
  /**
   * 测试 MCP 服务器：真的把进程拉起来并列出它的工具
   *
   * 这是"配置写对了吗"的唯一可靠答案——进程能否启动、协议是否对得上、
   * 到底暴露了哪些工具，光看配置文件是看不出来的。
   */
  const handleTestMcp = async (id: string) => {
    setMcpTesting(id);
    try {
      const status = await invoke<McpServerStatus>("test_mcp_server", { id });
      setMcpStatus((prev) => ({ ...prev, [id]: status }));
    } catch (e) {
      setMcpStatus((prev) => ({
        ...prev,
        [id]: { id, connected: false, tools: [], error: String(e) },
      }));
    } finally {
      setMcpTesting(null);
    }
  };

  const patchTools = (patch: Partial<ToolsConfig>) => {
    if (!config) return;
    const current = config.tools ?? DEFAULT_TOOLS_CONFIG;
    setConfig({ ...config, tools: { ...current, ...patch } });
  };

  const handleToggleWorkspaceWritable = async (w: WorkspaceView) => {
    setWorkspaceError("");
    try {
      // label 传 null 表示"保持不变"
      await invoke<WorkspaceView>("update_workspace", {
        id: w.id,
        label: null,
        writable: !w.writable,
      });
      await loadWorkspaces();
      await syncToolsFromBackend();
    } catch (e) {
      setWorkspaceError(errorText(e));
    }
  };

  const handleRemoveWorkspace = async (w: WorkspaceView) => {
    if (w.is_default) return; // 默认工作区不可删除（后端也会拒绝）
    if (!confirm(`确认移除工作区「${w.label}」？\n${w.path}`)) return;
    setWorkspaceError("");
    try {
      await invoke("remove_workspace", { id: w.id });
      await loadWorkspaces();
      await syncToolsFromBackend();
    } catch (e) {
      setWorkspaceError(errorText(e));
    }
  };

  const handleAddWorkspace = async () => {
    const path = newWorkspacePath.trim();
    if (!path) return;
    setAddingWorkspace(true);
    setWorkspaceError("");
    try {
      await invoke<WorkspaceView>("add_workspace", {
        path,
        label: newWorkspaceLabel.trim() || null,
        writable: newWorkspaceWritable,
      });
      setNewWorkspacePath("");
      setNewWorkspaceLabel("");
      setNewWorkspaceWritable(false);
      await loadWorkspaces();
      await syncToolsFromBackend();
    } catch (e) {
      // 后端返回的是中文校验信息，直接贴给用户看
      setWorkspaceError(errorText(e));
    } finally {
      setAddingWorkspace(false);
    }
  };

  // ─── 备份/导入 ─────────────────────────

  const downloadJson = (content: string, filename: string) => {
    const blob = new Blob([content], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = filename;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    // 立即 revoke 会让下载拿到空文件：必须等浏览器真正开始读取后再释放
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
  };

  const handleExportMemories = async () => {
    try {
      const json = await invoke<string>("export_memories");
      downloadJson(json, `kagami_no_konata_memories_${new Date().toISOString().split("T")[0]}.json`);
      setTestResult("✅ 记忆已导出");
    } catch (e) {
      setTestResult(`❌ 导出失败: ${e}`);
    }
  };

  const handleImportMemories = async () => {
    const input = document.createElement("input");
    input.type = "file";
    input.accept = ".json";
    input.onchange = async () => {
      const file = input.files?.[0];
      if (!file) return;
      try {
        const content = await file.text();
        // 覆盖式导入是不可撤销的破坏性操作，必须让用户显式选择，
        // 不能再把"取消"当作"清空后导入"（用户按 Esc / 点掉弹窗就会丢掉全部记忆）
        const replaceAll = confirm(
          "点「确定」将【清空现有记忆】后导入。\n" +
            "点「取消」则合并到现有记忆中（推荐）。"
        );
        const report = await invoke<ImportReport>("import_memories", {
          jsonContent: content,
          merge: !replaceAll,
        });
        setTestResult(`✅ ${describeReport(report, "条记忆")}`);
        loadMemories();
      } catch (e) {
        setTestResult(`❌ 导入失败: ${e}`);
      }
    };
    input.click();
  };

  const handleExportPersonas = async () => {
    try {
      const json = await invoke<string>("export_personas");
      downloadJson(json, `kagami_no_konata_personas_${new Date().toISOString().split("T")[0]}.json`);
      setTestResult("✅ 人格已导出");
    } catch (e) {
      setTestResult(`❌ 导出失败: ${e}`);
    }
  };

  const handleImportPersonas = async () => {
    const input = document.createElement("input");
    input.type = "file";
    input.accept = ".json";
    input.onchange = async () => {
      const file = input.files?.[0];
      if (!file) return;
      try {
        const content = await file.text();
        const report = await invoke<ImportReport>("import_personas", { jsonContent: content });
        setTestResult(`✅ ${describeReport(report, "个人格")}`);
      } catch (e) {
        setTestResult(`❌ 导入失败: ${e}`);
      }
    };
    input.click();
  };

  if (!config) return <div className="settings-page">加载中...</div>;

  const activeProvider = editingProvider;
  // 与默认值合并：即使后端返回的 tools 段缺字段也不会让输入框变成非受控
  const toolsConfig: ToolsConfig = { ...DEFAULT_TOOLS_CONFIG, ...(config.tools ?? {}) };

  return (
    <div className="settings-page">
      <div className="settings-header">
        <button className="back-btn" onClick={handleBack}>← 返回</button>
        <h2>⚙ 设置</h2>
      </div>

      {/* ─── 用户信息 ─── */}
      <div className="settings-section">
        <h3>👤 用户信息</h3>
        <label>
          <span>昵称</span>
          <input value={config.user.nickname}
            onChange={(e) => setConfig({ ...config, user: { ...config.user, nickname: e.target.value } })}
            placeholder="你希望此方怎么称呼你" />
        </label>
        <label>
          <span>性别</span>
          <select value={config.user.gender}
            onChange={(e) => setConfig({ ...config, user: { ...config.user, gender: e.target.value } })}>
            <option value="">未设置</option><option value="男">男</option><option value="女">女</option><option value="其他">其他</option>
          </select>
        </label>
        <label>
          <span>生日</span>
          <input type="date" value={config.user.birthday}
            onChange={(e) => setConfig({ ...config, user: { ...config.user, birthday: e.target.value } })} />
        </label>
        <label>
          <span>称呼代词</span>
          <input value={config.user.pronouns}
            onChange={(e) => setConfig({ ...config, user: { ...config.user, pronouns: e.target.value } })}
            placeholder="他/她/TA" />
        </label>
        <label>
          <span>简介</span>
          <input value={config.user.bio}
            onChange={(e) => setConfig({ ...config, user: { ...config.user, bio: e.target.value } })}
            placeholder="简单介绍一下自己（可选）" />
        </label>
      </div>

      {/* ─── LLM 提供商 ─── */}
      <div className="settings-section">
        <h3>🤖 LLM 提供商</h3>

        {/* 提供商列表 */}
        <div className="provider-list">
          {config.llm.providers.map((p) => (
            <div
              key={p.id}
              className={`provider-item ${p.id === config.llm.active_provider_id ? "active" : ""} ${p.id === editingProvider?.id ? "editing" : ""}`}
              onClick={() => setEditingProvider({ ...p })}
            >
              <div className="provider-info">
                {p.id === config.llm.active_provider_id && <span className="provider-star">★</span>}
                <span className="provider-name">{p.name}</span>
                <span className="provider-model">{p.model}</span>
              </div>
              <div className="provider-actions">
                {p.id !== config.llm.active_provider_id && (
                  <button className="provider-activate-btn" onClick={(e) => { e.stopPropagation(); handleSetActiveProvider(p.id); }}>
                    激活
                  </button>
                )}
                <button className="provider-delete-btn" onClick={(e) => { e.stopPropagation(); handleDeleteProvider(p.id); }}>
                  ×
                </button>
              </div>
            </div>
          ))}
        </div>

        {/* 添加提供商按钮 */}
        <button className="provider-add-btn" onClick={() => setShowAddModal(true)}>
          + 添加提供商
        </button>

        {/* 编辑面板 */}
        {activeProvider && (
          <div className="provider-editor">
            <h4>编辑：{activeProvider.name}</h4>
            <label>
              <span>名称</span>
              <input value={activeProvider.name}
                onChange={(e) => setEditingProvider({ ...activeProvider, name: e.target.value })} />
            </label>
            <label>
              <span>API URL</span>
              <input value={activeProvider.api_base_url}
                onChange={(e) => setEditingProvider({ ...activeProvider, api_base_url: e.target.value })} />
            </label>
            <label>
              <span>API Key</span>
              <input type="password" value={activeProvider.api_key}
                onChange={(e) => setEditingProvider({ ...activeProvider, api_key: e.target.value })} />
            </label>
            <label>
              <span>当前模型</span>
              <input value={activeProvider.model} readOnly className="model-current" />
            </label>
            <label>
              <span>嵌入模型</span>
              <input value={activeProvider.embedding_model}
                onChange={(e) => setEditingProvider({ ...activeProvider, embedding_model: e.target.value })} />
            </label>
            <label>
              <span>最大 Tokens</span>
              <input type="number" min="1" max="1000000" placeholder="留空 = 使用服务商默认"
                value={activeProvider.max_tokens ?? ""}
                onChange={(e) => setEditingProvider({ ...activeProvider, max_tokens: parseOptionalClamped(e.target.value, 1, 1_000_000) })} />
            </label>
            <label>
              <span>Temperature</span>
              <input type="number" step="0.1" min="0" max="2" value={activeProvider.temperature}
                onChange={(e) => setEditingProvider({ ...activeProvider, temperature: clampNumber(e.target.value, 0, 2, 0.8) })} />
            </label>
            <label>
              <span>启用思考</span>
              <input type="checkbox" checked={activeProvider.enable_thinking ?? false}
                onChange={(e) => setEditingProvider({ ...activeProvider, enable_thinking: e.target.checked })} />
              <span className="setting-hint">需要模型支持（如 DeepSeek-R1、QwQ 等）</span>
            </label>

            <div className="provider-editor-actions">
              <button onClick={handleSaveProvider} disabled={saving}>{saving ? "保存中..." : "💾 保存"}</button>
              <button onClick={handleTestProvider} disabled={testing}>{testing ? "测试中..." : "🔗 测试连接"}</button>
              <button onClick={handleFetchModels} disabled={fetching}>{fetching ? "获取中..." : "🔄 获取模型"}</button>
            </div>

            {/* 模型列表 */}
            {providerModels.length > 0 && (
              <div className="model-list">
                {providerModels.map((m) => {
                  const isEnabled = activeProvider.enabled_models.includes(m.id);
                  const isActive = activeProvider.model === m.id;
                  return (
                    <div key={m.id} className={`model-item ${isActive ? "active" : ""}`}>
                      <label className="model-checkbox">
                        <input type="checkbox" checked={isEnabled} onChange={() => toggleModelEnabled(m.id)} />
                        <span className="model-id">{m.id}</span>
                      </label>
                      <span className="model-owner">{m.owned_by ?? ""}</span>
                      {isActive && <span className="model-badge">当前</span>}
                      {/*
                        显式声明"支持深度思考"：名称启发式探测不准时（自建端点、新模型），
                        勾选后对话界面才会出现「深度思考」开关、请求里才带 enable_thinking
                      */}
                      <label
                        className="model-thinking-check"
                        title="声明该模型支持深度思考（覆盖名称探测）"
                      >
                        <input
                          type="checkbox"
                          checked={(activeProvider.thinking_models ?? []).includes(m.id)}
                          onChange={() => toggleModelThinking(m.id)}
                        />
                        深度思考
                      </label>
                      <button className="model-select-btn" onClick={() => handleSetActiveModel(m.id)} disabled={isActive}>
                        {isActive ? "✓" : "设为当前"}
                      </button>
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        )}
      </div>

      {/* ─── 模型路由（自动选择） ─── */}
      <div className="settings-section">
        <h3>🧭 模型路由（自动选择）</h3>
        <p className="settings-hint">
          任务会话里可以选「自动」：<b>Plan 模式与子代理优先用子模型</b>，
          <b>Work 模式优先用主模型</b>。配置只作用于选择了自动的会话 ——
          对话界面的底部工具条可以随时切回手动或跟随全局。
        </p>

        {selectableModels.length === 0 && (
          <p className="settings-hint">
            ⚠ 目前还没有「已启用」的模型：先在上面的提供商里点「🔄 获取模型」，
            勾选要用的模型并保存，这里才有可选的主/子模型。
          </p>
        )}

        <label>
          <span>新建任务会话默认自动</span>
          <input
            type="checkbox"
            checked={modelSettings.auto_by_default}
            onChange={(e) => updateModelSettings({ auto_by_default: e.target.checked })}
          />
          <span className="settings-hint">
            勾选后，新建的任务会话直接进入自动模式（普通对话会话不受影响）
          </span>
        </label>

        <label>
          <span>主模型</span>
          <select
            value={modelSettings.main ? modelRefValue(modelSettings.main) : ""}
            onChange={(e) =>
              updateModelSettings({ main: parseModelRefValue(e.target.value) })
            }
          >
            <option value="">跟随全局活跃提供商</option>
            {selectableModels.map((m) => (
              <option key={m.value} value={m.value}>
                {m.label}
              </option>
            ))}
          </select>
          <span className="settings-hint">Work 模式与普通轮次优先使用</span>
        </label>

        <div className="tool-subsection-title">子模型（按顺序轮转）</div>
        {modelSettings.subs.length === 0 ? (
          <p className="settings-hint">
            还没有子模型：Plan 模式与子代理会退回使用主模型。
          </p>
        ) : (
          <ul className="sub-model-list">
            {modelSettings.subs.map((sub, index) => (
              <li key={`${sub.provider_id}-${sub.model}`}>
                <span className="sub-model-order">{index + 1}</span>
                <span className="sub-model-name">
                  {sub.model}
                  <span className="sub-model-provider">
                    （
                    {config.llm.providers.find((p) => p.id === sub.provider_id)?.name ??
                      "提供商已删除"}
                    ）
                  </span>
                </span>
                <button type="button" onClick={() => moveSubModel(index, -1)} disabled={index === 0}>
                  ↑
                </button>
                <button
                  type="button"
                  onClick={() => moveSubModel(index, 1)}
                  disabled={index === modelSettings.subs.length - 1}
                >
                  ↓
                </button>
                <button type="button" onClick={() => removeSubModel(index)}>
                  ✕
                </button>
              </li>
            ))}
          </ul>
        )}

        <label>
          <span>添加子模型</span>
          <select
            value=""
            onChange={(e) => {
              addSubModel(e.target.value);
              e.target.value = "";
            }}
          >
            <option value="">选择模型…</option>
            {selectableModels
              .filter(
                (m) =>
                  !modelSettings.subs.some(
                    (s) => modelRefValue(s) === m.value
                  )
              )
              .map((m) => (
                <option key={m.value} value={m.value}>
                  {m.label}
                </option>
              ))}
          </select>
          <span className="settings-hint">
            最多 {MAX_SUB_MODELS} 个；多个子模型会轮流分配给各个子代理
          </span>
        </label>

        <p className="settings-hint">
          模型的「深度思考」开关出现在对话界面，且只在该模型被判定支持时显示：
          名称探测不准时，可在上方提供商模型列表里勾选「深度思考」显式声明。
        </p>
      </div>

      {/* ─── 记忆系统 ─── */}
      <div className="settings-section">
        <h3>🧠 记忆系统</h3>
        <label>
          <span>启用记忆</span>
          <input type="checkbox" checked={config.memory.enabled}
            onChange={(e) => setConfig({ ...config, memory: { ...config.memory, enabled: e.target.checked } })} />
        </label>
        <label>
          <span>自动提取</span>
          <input type="checkbox" checked={config.memory.auto_extract}
            onChange={(e) => setConfig({ ...config, memory: { ...config.memory, auto_extract: e.target.checked } })} />
        </label>
        <label>
          <span>上下文记忆数</span>
          <input type="number" min="0" max="100" value={config.memory.max_context_memories}
            onChange={(e) => setConfig({ ...config, memory: { ...config.memory, max_context_memories: clampNumber(e.target.value, 0, 100, 5) } })} />
        </label>
        {memories.length > 0 && (
          <div className="memory-list">
            <div className="memory-list-header">
              <span>已存储 {memories.length} 条记忆</span>
              <button className="memory-clear-btn" onClick={handleClearMemories}>清空</button>
            </div>
            {memories.map((m) => (
              <div key={m.id} className="memory-item">
                <div className="memory-content">{m.content}</div>
                <div className="memory-meta">
                  <span className="memory-type">{m.memory_type}</span>
                  <span className="memory-importance">重要性: {(m.importance * 100).toFixed(0)}%</span>
                </div>
                <button className="memory-delete-btn" onClick={() => handleDeleteMemory(m.id)}>×</button>
              </div>
            ))}
          </div>
        )}
        {memories.length === 0 && config.memory.enabled && (
          <div className="memory-empty">暂无记忆，对话后将自动提取</div>
        )}
      </div>

      {/* ─── 工具 ─── */}
      <div className="settings-section">
        <h3>🛠 工具</h3>
        <label>
          <span>启用工具</span>
          <input
            type="checkbox"
            checked={toolsConfig.enabled}
            onChange={(e) => patchTools({ enabled: e.target.checked })}
          />
          <span className="settings-hint">
            允许角色读写文件、执行命令等（悬浮窗始终不参与工具调用）
          </span>
        </label>
        <label>
          <span>权限模式</span>
          <select
            value={toolsConfig.mode}
            disabled={!toolsConfig.enabled}
            onChange={(e) =>
              patchTools({ mode: e.target.value as ToolsConfig["mode"] })
            }
          >
            <option value="read_only">只读</option>
            <option value="standard">标准</option>
            <option value="full">完整</option>
          </select>
          <span className="settings-hint">
            {TOOL_MODE_HINT[toolsConfig.mode] ?? ""}
            <br />
            {TASK_MODE_HINT}
          </span>
        </label>
        <label>
          <span>工具步数上限</span>
          <input
            type="number"
            min="1"
            max="128"
            disabled={!toolsConfig.enabled}
            value={toolsConfig.max_steps}
            onChange={(e) =>
              patchTools({
                max_steps: clampNumber(e.target.value, 1, 128, 32),
              })
            }
          />
          <span className="settings-hint">
            每次生成允许的最大工具轮数（默认 32，上限 128）。任务会话的 Plan 与 Work
            共用这个预算（保底 20 轮），普通聊天固定不超过 3 轮；一轮可以并行执行
            同一条回复里的多个只读调用
          </span>
        </label>
        <label>
          <span>工作记忆</span>
          <input
            type="checkbox"
            disabled={!toolsConfig.enabled}
            checked={toolsConfig.working_memory}
            onChange={(e) => patchTools({ working_memory: e.target.checked })}
          />
          <span className="settings-hint">
            允许角色用 save_note 记下跨轮结论（例如「认证在 auth.rs:42」），
            避免每轮重复调查。笔记有长度与条数上限、注入时带「不可信」标记，
            界面上可随时查看与清空；关闭后相关工具会明确报错且不再注入
          </span>
        </label>
        <label>
          <span>单次工具超时（秒）</span>
          <input
            type="number"
            min="5"
            max="1800"
            step="5"
            disabled={!toolsConfig.enabled}
            value={toolsConfig.call_timeout_secs}
            onChange={(e) =>
              // 后端校验范围是 5 ~ 1800 秒；首次编译这类长命令需要更大的预算
              patchTools({
                call_timeout_secs: clampNumber(e.target.value, 5, 1800, 180),
              })
            }
          />
           <span className="settings-hint">
            执行命令的等待上限。超时不会直接报错：会带着已经产生的输出提前结束，
            并把「跑到哪一步超时」如实汇报（首次编译建议 300 秒以上）
          </span>
        </label>

        {/* 子代理预算：调查类任务的开销上限，直接决定"后台偷偷花了多少" */}
        <label>
          <span>子代理名额（每轮）</span>
          <input
            type="number"
            min="1"
            max="4"
            disabled={!toolsConfig.enabled}
            value={toolsConfig.subagent_max_children}
            onChange={(e) =>
              patchTools({
                subagent_max_children: clampNumber(e.target.value, 1, 4, 2),
              })
            }
          />
          <span className="settings-hint">
            每轮生成最多派出几个只读子代理（默认 2）。派多了会显著增加 token 消耗
          </span>
        </label>
        <label>
          <span>子代理步数</span>
          <input
            type="number"
            min="1"
            max="64"
            disabled={!toolsConfig.enabled}
            value={toolsConfig.subagent_steps}
            onChange={(e) =>
              patchTools({
                subagent_steps: clampNumber(e.target.value, 1, 64, 32),
              })
            }
          />
          <span className="settings-hint">
            每个子代理自己的工具轮数上限（默认 32，与主循环一致）；子代理始终只读
          </span>
        </label>
        <label>
          <span>单次子代理任务数</span>
          <input
            type="number"
            min="1"
            max="3"
            disabled={!toolsConfig.enabled}
            value={toolsConfig.subagent_max_tasks}
            onChange={(e) =>
              patchTools({
                subagent_max_tasks: clampNumber(e.target.value, 1, 3, 3),
              })
            }
          />
          <span className="settings-hint">
            一次 spawn_subagents 调用最多提交几个调查任务（默认 3，上限由工具声明固定为 3）
          </span>
        </label>
        <label>
          <span>子代理时间预算（秒）</span>
          <input
            type="number"
            min="30"
            max="3600"
            step="30"
            disabled={!toolsConfig.enabled}
            value={toolsConfig.subagent_timeout_secs}
            onChange={(e) =>
              patchTools({
                subagent_timeout_secs: clampNumber(e.target.value, 30, 3600, 600),
              })
            }
          />
          <span className="settings-hint">
            整批子代理的墙钟时间上限（默认 600 秒）。到点前会把已完成的结论带回来并注明
            「时间预算用尽」，而不是被强制掐断；普通工具仍用上面的单次超时
          </span>
        </label>

        {/* 工具清单：让用户知道 agent 到底能做什么 */}
        <div className="tool-list">
          {tools.length === 0 ? (
            <div className="tool-list-empty">暂无可用工具</div>
          ) : (
            tools.map((t) => (
              <div
                key={t.name}
                className={`tool-item ${t.enabled ? "" : "disabled"}`}
              >
                <div className="tool-item-main">
                  <span className="tool-item-label">{t.label}</span>
                  <span className="tool-item-name">{t.name}</span>
                  <span
                    className={`tool-permission-badge ${normalizePermission(t.permission)}`}
                  >
                    {TOOL_PERMISSION_LABEL[normalizePermission(t.permission)]}
                  </span>
                  {t.read_only && <span className="tool-item-tag">只读</span>}
                  {!t.enabled && <span className="tool-item-tag off">已停用</span>}
                </div>
                <div className="tool-item-desc">{t.description}</div>
              </div>
            ))
          )}
        </div>

        {/* 联网检索 */}
        <h4 className="tool-subsection-title">🔎 联网检索</h4>
        <label>
          <span>启用联网检索</span>
          <input
            type="checkbox"
            disabled={!toolsConfig.enabled}
            checked={toolsConfig.search.enabled}
            onChange={(e) =>
              patchTools({ search: { ...toolsConfig.search, enabled: e.target.checked } })
            }
          />
          <span className="settings-hint">
            这是唯一会把你的提问内容发给第三方的能力，因此默认关闭。
            检索只返回候选链接，正文仍需用 web_fetch 打开并受域名白名单限制
          </span>
        </label>
        <label>
          <span>检索后端</span>
          <select
            disabled={!toolsConfig.enabled || !toolsConfig.search.enabled}
            value={toolsConfig.search.provider}
            onChange={(e) =>
              patchTools({
                search: { ...toolsConfig.search, provider: e.target.value as SearchProvider },
              })
            }
          >
            <option value="searxng">SearXNG（自建）</option>
            <option value="tavily">Tavily</option>
            <option value="brave">Brave Search</option>
          </select>
          <span className="settings-hint">{SEARCH_PROVIDER_HINT[toolsConfig.search.provider]}</span>
        </label>
        <label>
          <span>检索端点</span>
          <input
            disabled={!toolsConfig.enabled || !toolsConfig.search.enabled}
            value={toolsConfig.search.endpoint}
            placeholder="https://searx.example.com/search"
            onChange={(e) =>
              patchTools({ search: { ...toolsConfig.search, endpoint: e.target.value } })
            }
          />
        </label>
        <label>
          <span>检索 API Key</span>
          <input
            type="password"
            disabled={!toolsConfig.enabled || !toolsConfig.search.enabled}
            value={toolsConfig.search.api_key}
            placeholder="自建实例可留空"
            onChange={(e) =>
              patchTools({ search: { ...toolsConfig.search, api_key: e.target.value } })
            }
          />
        </label>
        <label>
          <span>返回条数</span>
          <input
            type="number"
            min="1"
            max="10"
            disabled={!toolsConfig.enabled || !toolsConfig.search.enabled}
            value={toolsConfig.search.max_results}
            onChange={(e) =>
              patchTools({
                search: {
                  ...toolsConfig.search,
                  max_results: clampNumber(e.target.value, 1, 10, 5),
                },
              })
            }
          />
        </label>

        {/* MCP 服务器 */}
        <h4 className="tool-subsection-title">🔌 MCP 服务器</h4>
        <p className="settings-hint">
          外部工具生态：服务器命令写在 <code>config.json</code> 的 <code>tools.mcp.servers</code> 里，
          必须同时打开 <code>enabled</code> 与 <code>trusted</code> 才会被启动（加配置不等于授权）。
          启动的服务器的工具会以 <code>mcp:服务器:工具</code> 出现在工具表里，权限按服务器映射
        </p>
        <div className="mcp-list">
          {toolsConfig.mcp.servers.length === 0 ? (
            <div className="tool-list-empty">
              尚未配置 MCP 服务器（在 config.json 里添加 tools.mcp.servers 后重启应用）
            </div>
          ) : (
            toolsConfig.mcp.servers.map((server) => {
              const status = mcpStatus[server.id];
              return (
                <div key={server.id} className={`mcp-item ${server.enabled ? "" : "disabled"}`}>
                  <div className="mcp-item-main">
                    <span className="mcp-item-id">{server.id}</span>
                    <span className={`tool-permission-badge ${server.permission}`}>
                      {server.permission === "read"
                        ? "只读"
                        : server.permission === "write"
                          ? "写入"
                          : "执行"}
                    </span>
                    {!server.enabled && <span className="tool-item-tag off">已停用</span>}
                    {server.enabled && !server.trusted && (
                      <span className="tool-item-tag off">未信任</span>
                    )}
                  </div>
                  <div className="mcp-item-cmd">
                    {server.command} {server.args.join(" ")}
                  </div>
                  <div className="mcp-item-hint">{MCP_PERMISSION_HINT[server.permission]}</div>
                  <div className="mcp-item-actions">
                    <button
                      className="mcp-test-btn"
                      disabled={mcpTesting === server.id}
                      onClick={() => void handleTestMcp(server.id)}
                    >
                      {mcpTesting === server.id ? "连接中…" : "测试连接"}
                    </button>
                    {status && (
                      <span className={`mcp-status ${status.connected ? "ok" : "error"}`}>
                        {status.connected
                          ? `已连接 · ${status.tools.length} 个工具：${status.tools.join("、")}`
                          : status.error ?? "连接失败"}
                      </span>
                    )}
                  </div>
                </div>
              );
            })
          )}
        </div>

        {/* 工作区 */}
        <h4 className="tool-subsection-title">📁 工作区</h4>
        <p className="settings-hint">
          工具只能在这些根目录内读写文件；「已失效」表示路径当前不可用
        </p>
        <div className="workspace-list">
          {workspaces.length === 0 ? (
            <div className="tool-list-empty">暂无工作区</div>
          ) : (
            workspaces.map((w) => (
              <div
                key={w.id}
                className={`workspace-item ${w.available ? "" : "unavailable"}`}
              >
                <div className="workspace-info">
                  <div className="workspace-title">
                    <span className="workspace-label">{w.label}</span>
                    {w.is_default && (
                      <span className="workspace-tag">默认</span>
                    )}
                  </div>
                  <span className="workspace-path" title={w.path}>
                    {w.path}
                  </span>
                </div>
                <div className="workspace-actions">
                  <span
                    className={`workspace-availability ${w.available ? "ok" : "bad"}`}
                  >
                    {w.available ? "可用" : "已失效"}
                  </span>
                  <button
                    className={`workspace-writable-btn ${w.writable ? "on" : ""}`}
                    onClick={() => handleToggleWorkspaceWritable(w)}
                    title={w.writable ? "点击改为只读" : "点击允许写入"}
                  >
                    {w.writable ? "可写" : "只读"}
                  </button>
                  {/* 默认根目录不可删除：后端也会拒绝 */}
                  {!w.is_default && (
                    <button
                      className="workspace-remove-btn"
                      onClick={() => handleRemoveWorkspace(w)}
                      title="移除该工作区"
                    >
                      ×
                    </button>
                  )}
                </div>
              </div>
            ))
          )}
        </div>

        {/* 添加工作区 */}
        <div className="workspace-add">
          <input
            className="workspace-path-input"
            value={newWorkspacePath}
            onChange={(e) => setNewWorkspacePath(e.target.value)}
            placeholder="绝对路径，如 D:\projects\demo"
            spellCheck={false}
          />
          <input
            className="workspace-label-input"
            value={newWorkspaceLabel}
            onChange={(e) => setNewWorkspaceLabel(e.target.value)}
            placeholder="备注名（可选）"
          />
          <label className="workspace-writable-check">
            <input
              type="checkbox"
              checked={newWorkspaceWritable}
              onChange={(e) => setNewWorkspaceWritable(e.target.checked)}
            />
            <span>可写</span>
          </label>
          <button
            className="workspace-add-btn"
            onClick={handleAddWorkspace}
            disabled={!newWorkspacePath.trim() || addingWorkspace}
          >
            {addingWorkspace ? "添加中..." : "+ 添加"}
          </button>
        </div>

        {workspaceError && (
          <div className="workspace-error">⚠ {workspaceError}</div>
        )}
      </div>

      {/* ─── 外观 ─── */}
      <div className="settings-section">
        <h3>🎨 外观</h3>
        <label>
          <span>主题</span>
          <select value={config.ui.theme} onChange={(e) => {
            const newConfig = { ...config, ui: { ...config.ui, theme: e.target.value } };
            setConfig(newConfig);
            const root = document.documentElement;
            if (e.target.value === "auto") {
              root.setAttribute("data-theme", window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
            } else {
              root.setAttribute("data-theme", e.target.value);
            }
          }}>
            <option value="dark">暗色</option><option value="light">亮色</option><option value="auto">跟随系统</option>
          </select>
        </label>
        <label>
          <span>字号</span>
          <input type="range" min="12" max="18" step="1" value={config.ui.font_size}
            onChange={(e) => {
              const size = Number(e.target.value);
              setConfig({ ...config, ui: { ...config.ui, font_size: size } });
              document.documentElement.style.fontSize = `${size}px`;
            }} />
          <span className="font-size-label">{config.ui.font_size}px</span>
        </label>
        <label>
          <span>显示消息统计</span>
          <input type="checkbox" checked={config.ui.show_message_stats ?? false}
            onChange={(e) => setConfig({ ...config, ui: { ...config.ui, show_message_stats: e.target.checked } })} />
          <span className="settings-hint">在每条回复下显示 Token 用量和响应时间</span>
        </label>
      </div>

      {/* ─── 页面逻辑 ─── */}
      <div className="settings-section">
        <h3>🪟 页面逻辑</h3>
        <label>
          <span>悬浮窗时钟</span>
          <input type="checkbox" checked={config.ui.show_float_clock ?? false}
            onChange={(e) => setConfig({ ...config, ui: { ...config.ui, show_float_clock: e.target.checked } })} />
          <span className="settings-hint">在桌面宠物左上角显示时间和日期</span>
        </label>
        <label>
          <span>悬浮窗弹出位置</span>
          <select value={config.ui.float_position ?? "bottom-right"} onChange={(e) => {
            setConfig({ ...config, ui: { ...config.ui, float_position: e.target.value } });
          }}>
            <option value="bottom-right">右下角（默认）</option>
            <option value="bottom-left">左下角</option>
            <option value="top-right">右上角</option>
            <option value="top-left">左上角</option>
            <option value="top-center">顶部居中</option>
            <option value="bottom-center">底部居中</option>
          </select>
          <span className="settings-hint">悬浮窗每次出现时的屏幕位置</span>
        </label>
        <label>
          <span>关闭主窗口时</span>
          <select value={config.ui.close_action ?? "hide"} onChange={(e) => {
            setConfig({ ...config, ui: { ...config.ui, close_action: e.target.value } });
          }}>
            <option value="exit">退出程序</option>
            <option value="hide">进入后台（默认）</option>
            <option value="hide_and_float">进入后台并打开悬浮窗</option>
          </select>
          <span className="settings-hint">
            {(config.ui.close_action ?? "hide") === "exit" && "点击关闭按钮将直接退出程序"}
            {(config.ui.close_action ?? "hide") === "hide" && "点击关闭按钮将隐藏到系统托盘，可通过托盘菜单恢复"}
            {(config.ui.close_action ?? "hide") === "hide_and_float" && "点击关闭按钮将隐藏主窗口并自动打开桌面悬浮窗"}
          </span>
        </label>
      </div>

      {/* ─── 戳一下设置 ─── */}
      <div className="settings-section">
        <h3>👆 戳一下</h3>
        <label>
          <span>启用戳一下</span>
          <input type="checkbox" checked={config.ui.poke_enabled ?? true}
            onChange={(e) => setConfig({ ...config, ui: { ...config.ui, poke_enabled: e.target.checked } })} />
          <span className="setting-hint">点击桌面宠物时概率触发反应</span>
        </label>
        <label>
          <span>触发概率</span>
          <input type="range" min="0" max="1" step="0.05"
            value={config.ui.poke_probability ?? 0.3}
            onChange={(e) => setConfig({ ...config, ui: { ...config.ui, poke_probability: Number(e.target.value) } })} />
          <span className="setting-hint">{Math.round((config.ui.poke_probability ?? 0.3) * 100)}%</span>
        </label>
        <label>
          <span>LLM 反应概率</span>
          <input type="range" min="0" max="1" step="0.05"
            value={config.ui.poke_llm_chance ?? 0.15}
            onChange={(e) => setConfig({ ...config, ui: { ...config.ui, poke_llm_chance: Number(e.target.value) } })} />
          <span className="setting-hint">{Math.round((config.ui.poke_llm_chance ?? 0.15) * 100)}%</span>
        </label>
      </div>

      {/* ─── 气泡设置 ─── */}
      <div className="settings-section">
        <h3>💬 气泡设置</h3>
        <label>
          <span>自动隐藏时间</span>
          <input type="number" min="0" max="3600" step="1"
            value={config.ui.bubble_auto_hide_secs ?? 20}
            onChange={(e) => setConfig({ ...config, ui: { ...config.ui, bubble_auto_hide_secs: clampNumber(e.target.value, 0, 3600, 20) } })} />
          <span className="setting-hint">秒（设为 0 则不自动隐藏）</span>
        </label>
      </div>

      {/* ─── 人格管理 ─── */}
      <div className="settings-section">
        <h3>🎭 人格管理</h3>
        <p className="settings-hint">编辑角色设定、系统提示词和性格特征</p>
        <button className="persona-editor-btn" onClick={() => setCurrentPage("persona")}>✦ 打开人格编辑器</button>
      </div>

      {/* ─── 备份与导入 ─── */}
      <div className="settings-section">
        <h3>💾 备份与导入</h3>
        <p className="settings-hint">导出数据用于迁移或备份，导入时可选择合并或替换</p>
        <div className="backup-actions">
          <div className="backup-group">
            <span className="backup-label">记忆数据</span>
            <button className="backup-btn" onClick={handleExportMemories}>📤 导出记忆</button>
            <button className="backup-btn" onClick={handleImportMemories}>📥 导入记忆</button>
          </div>
          <div className="backup-group">
            <span className="backup-label">人格配置</span>
            <button className="backup-btn" onClick={handleExportPersonas}>📤 导出人格</button>
            <button className="backup-btn" onClick={handleImportPersonas}>📥 导入人格</button>
          </div>
        </div>
      </div>

      {/* ─── 使用统计 ─── */}
      <div className="settings-section">
        <h3>📊 使用统计</h3>
        {stats ? (
          <div className="stats-grid">
            <div className="stats-item"><span className="stats-value">{stats.total_requests}</span><span className="stats-label">请求次数</span></div>
            <div className="stats-item"><span className="stats-value">{stats.total_tokens.toLocaleString()}</span><span className="stats-label">总 Token</span></div>
            <div className="stats-item"><span className="stats-value">{stats.prompt_tokens.toLocaleString()}</span><span className="stats-label">输入 Token</span></div>
            <div className="stats-item"><span className="stats-value">{stats.completion_tokens.toLocaleString()}</span><span className="stats-label">输出 Token</span></div>
            <div className="stats-item"><span className="stats-value">{stats.total_time_ms > 60000 ? `${(stats.total_time_ms / 60000).toFixed(1)} 分钟` : `${(stats.total_time_ms / 1000).toFixed(1)} 秒`}</span><span className="stats-label">总耗时</span></div>
            <div className="stats-item"><span className="stats-value">{stats.total_requests > 0 ? Math.round(stats.total_tokens / stats.total_requests) : 0}</span><span className="stats-label">平均 Token/次</span></div>
          </div>
        ) : (
          <div className="stats-loading">加载中...</div>
        )}
        <button className="stats-reset-btn" onClick={async () => {
          if (!confirm("确认重置所有统计数据？")) return;
          await invoke("reset_usage_stats");
          invoke<UsageStats>("get_usage_stats").then(setStats);
        }}>🔄 重置统计</button>
      </div>

      {/* ─── 保存/测试 ─── */}
      <div className="settings-actions">
        <button onClick={async () => {
          if (!config) return;
          setSaving(true);
          try {
            await invoke("update_config", { newConfig: config });
            configSnapshotRef.current = sections(config);
            setTestResult("✅ 配置已保存");
          } catch (e) { setTestResult(`❌ 保存失败: ${e}`); }
          finally { setSaving(false); }
        }} disabled={saving}>{saving ? "保存中..." : "💾 保存全局配置"}</button>
        {configDirty && <span className="settings-unsaved-hint">● 有未保存的修改</span>}
      </div>

      {testResult && <div className="test-result">{testResult}</div>}

      {/* ─── 添加提供商弹窗 ─── */}
      {showAddModal && (
        <div className="modal-overlay" onClick={() => setShowAddModal(false)}>
          <div className="modal-card" onClick={(e) => e.stopPropagation()}>
            <h3>➕ 添加 API 提供商</h3>
            <label>
              <span>名称</span>
              <input
                value={newProviderName}
                onChange={(e) => setNewProviderName(e.target.value)}
                placeholder="如 OpenAI、DeepSeek、Ollama"
              />
            </label>
            <label>
              <span>API Base URL</span>
              <input
                value={newProviderUrl}
                onChange={(e) => setNewProviderUrl(e.target.value)}
                placeholder="https://api.openai.com/v1"
              />
            </label>
            <label>
              <span>API Key</span>
              <input
                type="password"
                value={newProviderKey}
                onChange={(e) => setNewProviderKey(e.target.value)}
                placeholder="sk-...（本地模型可留空）"
              />
            </label>
            <div className="modal-actions">
              <button className="modal-cancel-btn" onClick={() => setShowAddModal(false)}>
                取消
              </button>
              <button
                className="modal-confirm-btn"
                onClick={handleAddProvider}
                disabled={!newProviderName || !newProviderUrl}
              >
                添加
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
