import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useChatStore } from "../../stores/chatStore";
import {
  TOOL_PERMISSION_LABEL,
  normalizePermission,
  type ToolInfo,
  type WorkspaceView,
} from "../../types/tools";

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
  max_steps: 8,
  max_output_bytes: 65536,
  approval_timeout_secs: 120,
  command_allowlist: [],
  web_domain_allowlist: [],
};

const TOOL_MODE_HINT: Record<ToolsConfig["mode"], string> = {
  read_only: "只读：写入与执行类工具直接从工具表中移除",
  standard: "标准：读与App内写入自动放行，写文件/执行命令需要审批",
  full: "完整：在标准之上放开命令执行（仍禁止敏感命令）",
};

interface LlmProvider {
  id: string;
  name: string;
  api_base_url: string;
  api_key: string;
  model: string;
  enabled_models: string[];
  embedding_model: string;
  max_tokens: number;
  temperature: number;
  enable_thinking?: boolean;
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
    JSON.stringify({ user: c.user, memory: c.memory, ui: c.ui, tools: c.tools });

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
      downloadJson(json, `konata_memories_${new Date().toISOString().split("T")[0]}.json`);
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
      downloadJson(json, `konata_personas_${new Date().toISOString().split("T")[0]}.json`);
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
              <input type="number" min="1" max="1000000" value={activeProvider.max_tokens}
                onChange={(e) => setEditingProvider({ ...activeProvider, max_tokens: clampNumber(e.target.value, 1, 1_000_000, 2048) })} />
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
