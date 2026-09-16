/**
 * 模型选择与自动选择的类型定义
 *
 * 与后端一一对应（改这里必须同步改 Rust 侧，否则 Tauri 的参数反序列化会静默失败）：
 * - `ModelMode` / `ModelRef` / `ModelSettings` → `config::types`
 * - `SessionModelPref` → `llm::router::SessionModelPref`（存于 `sessions.model_pref`）
 * - `ModelCatalog` → `commands::settings::ModelCatalog`（`get_model_catalog` 命令）
 */

/** 模型选择模式 */
export type ModelMode = "inherit" | "manual" | "auto";

/** 指向"某个提供商的某个模型"的稳定引用 */
export interface ModelRef {
  provider_id: string;
  model: string;
}

/**
 * 会话级模型偏好
 *
 * `mode` 为 `inherit` 或整段为 `null` 时表示跟随全局活跃提供商。
 * `thinking` 为 `null`/缺省表示跟随提供商默认（设置页里的"深度思考"开关）。
 */
export interface SessionModelPref {
  mode: ModelMode;
  provider_id?: string | null;
  model?: string | null;
  thinking?: boolean | null;
}

/** 一个可选模型（能力探测由后端完成，界面不重复实现启发式） */
export interface ModelOption {
  id: string;
  /** 是否支持深度思考：为 false 时界面不显示"深度思考"开关 */
  supports_thinking: boolean;
  is_current: boolean;
}

/** 一个提供商下的可选模型（只含已启用的模型 + 当前模型） */
export interface ProviderModels {
  provider_id: string;
  provider_name: string;
  is_active: boolean;
  /** 地址与密钥齐备（否则选中也无法真正调用） */
  is_usable: boolean;
  current_model: string;
  /** 提供商级"默认开启思考"：会话未单独设置时的实际取值 */
  thinking_default: boolean;
  models: ModelOption[];
}

/** 自动选择的主/子模型池（与后端 `AppConfig.models` 对应） */
export interface ModelSettings {
  /** 新建任务会话是否默认开启自动选择 */
  auto_by_default: boolean;
  main: ModelRef | null;
  /** 子模型池：Plan 模式与子代理优先使用，按顺序轮转 */
  subs: ModelRef[];
}

/** `get_model_catalog` 的返回值 */
export interface ModelCatalog {
  providers: ProviderModels[];
  settings: ModelSettings;
}

/** 主/子模型引用的展示文案（设置页与对话界面共用） */
export function modelRefLabel(catalog: ModelCatalog | null, ref: ModelRef | null): string {
  if (!ref) return "未设置";
  const provider = catalog?.providers.find((p) => p.provider_id === ref.provider_id);
  return provider ? `${ref.model}（${provider.provider_name}）` : `${ref.model}（提供商已删除）`;
}

/** 模型引用 → 下拉框的 value（`providerId\u0000model`，避免拼接歧义） */
export function modelRefValue(ref: ModelRef): string {
  return `${ref.provider_id}\u0000${ref.model}`;
}

/** 下拉框 value → 模型引用（格式不符时返回 null） */
export function parseModelRefValue(value: string): ModelRef | null {
  const index = value.indexOf("\u0000");
  if (index <= 0) return null;
  return {
    provider_id: value.slice(0, index),
    model: value.slice(index + 1),
  };
}

/** 把（提供商, 模型）在目录里查出来（拿能力探测结果用） */
export function findModelOption(
  catalog: ModelCatalog | null,
  providerId?: string | null,
  modelId?: string | null
): ModelOption | null {
  if (!catalog || !providerId || !modelId) return null;
  const provider = catalog.providers.find((p) => p.provider_id === providerId);
  return provider?.models.find((m) => m.id === modelId) ?? null;
}

/** 目录里当前活跃提供商的模型（"跟随全局"时实际用的那个） */
export function activeProviderModels(catalog: ModelCatalog | null): ProviderModels | null {
  if (!catalog) return null;
  return catalog.providers.find((p) => p.is_active) ?? catalog.providers[0] ?? null;
}
