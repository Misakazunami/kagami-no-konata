import { useEffect, useMemo } from "react";
import { useChatStore } from "../../stores/chatStore";
import {
  activeProviderModels,
  findModelOption,
  modelRefLabel,
  modelRefValue,
  parseModelRefValue,
  type ModelRef,
  type SessionModelPref,
} from "../../types/models";

/** 未设置模型时的占位（"跟随全局提供商"） */
const INHERIT_VALUE = "__inherit__";

/**
 * 对话界面底部的模型选择条
 *
 * 需求对应关系（三条都必须"实时"生效，即只影响下一轮，不打断正在跑的生成）：
 * 1. 选择模型：下拉框选定 → 写会话级偏好（`manual`）
 * 2. 开关深度思考：仅当该模型**被判定支持**时才显示（有该能力才给开关）
 * 3. 自动选择：**只对任务会话**显示，切换到 `auto` 后按设置里的主/子模型路由
 *
 * 为什么把"能力探测"和"本轮用哪个模型"都交给后端算：界面不重复实现一套启发式，
 * 也避免"界面显示的支持情况"与"实际发出去的请求"对不上。
 */
export function ModelBar() {
  const catalog = useChatStore((s) => s.modelCatalog);
  const loadModelCatalog = useChatStore((s) => s.loadModelCatalog);
  const setSessionModel = useChatStore((s) => s.setSessionModel);
  const sessions = useChatStore((s) => s.sessions);
  const currentSessionId = useChatStore((s) => s.currentSessionId);
  const setCurrentPage = useChatStore((s) => s.setCurrentPage);

  useEffect(() => {
    void loadModelCatalog();
  }, [loadModelCatalog]);

  const session = sessions.find((s) => s.id === currentSessionId);
  const isTaskSession = session?.session_type === "task";
  const taskMode = session?.task_mode ?? "plan";
  const pref = session?.model_pref ?? null;
  const mode = pref?.mode ?? "inherit";
  const settings = catalog?.settings ?? null;

  /** 自动选择时，本轮主轮次实际用的模型（Plan 用第一个子模型，Work 用主模型） */
  const autoEffectiveRef: ModelRef | null = useMemo(() => {
    if (!settings) return null;
    if (isTaskSession && taskMode === "plan" && settings.subs.length > 0) {
      return settings.subs[0];
    }
    return settings.main ?? null;
  }, [settings, isTaskSession, taskMode]);

  /** 当前生效的模型引用（用于决定是否显示"深度思考"开关） */
  const effectiveRef: ModelRef | null = useMemo(() => {
    if (mode === "manual") {
      return pref?.provider_id && pref?.model
        ? { provider_id: pref.provider_id, model: pref.model }
        : null;
    }
    if (mode === "auto") return autoEffectiveRef;
    const active = activeProviderModels(catalog);
    return active ? { provider_id: active.provider_id, model: active.current_model } : null;
  }, [mode, pref, autoEffectiveRef, catalog]);

  const effectiveOption = findModelOption(
    catalog,
    effectiveRef?.provider_id,
    effectiveRef?.model
  );
  // 只有"确实支持深度思考"的模型才显示开关：对不支持的模型下发 enable_thinking
  // 会被严格端点直接拒绝（见 `llm::capabilities`）。
  const showThinkingToggle = effectiveOption?.supports_thinking === true;
  // 会话没有单独设置时，实际取值来自提供商级默认（设置页里的"启用思考"）
  const providerThinkingDefault =
    catalog?.providers.find((p) => p.provider_id === effectiveRef?.provider_id)
      ?.thinking_default ?? false;
  const thinkingChecked = pref?.thinking ?? providerThinkingDefault;

  const applyPref = (next: SessionModelPref | null) => {
    if (!currentSessionId) return;
    void setSessionModel(currentSessionId, next);
  };

  const handleModelChange = (value: string) => {
    if (value === INHERIT_VALUE) {
      // "跟随全局" = 清掉手动选择，但保留深度思考开关（它描述的是本会话的意愿）
      applyPref({ mode: "inherit", thinking: pref?.thinking ?? null });
      return;
    }
    const ref = parseModelRefValue(value);
    if (!ref) return;
    applyPref({
      mode: "manual",
      provider_id: ref.provider_id,
      model: ref.model,
      thinking: pref?.thinking ?? null,
    });
  };

  const handleThinkingChange = (checked: boolean) => {
    applyPref({
      mode,
      provider_id: pref?.provider_id ?? null,
      model: pref?.model ?? null,
      thinking: checked,
    });
  };

  const handleToggleAuto = () => {
    if (mode === "auto") {
      // 关闭自动 → 回到跟随全局；深度思考是本会话的意愿，必须保留
      // （历史实现直接 applyPref(null)，把用户刚开的思考开关一起清掉）
      applyPref({ mode: "inherit", thinking: pref?.thinking ?? null });
    } else {
      applyPref({ mode: "auto", thinking: pref?.thinking ?? null });
    }
  };

  // 目录还没加载出来时（或加载失败）不占地方：不显示一个空壳工具条
  if (!catalog) return null;

  const manualValue =
    mode === "manual" && pref?.provider_id && pref?.model
      ? modelRefValue({ provider_id: pref.provider_id, model: pref.model })
      : INHERIT_VALUE;

  return (
    <div className="model-bar">
      <span className="model-bar-label">模型</span>

      {isTaskSession && (
        <button
          type="button"
          className={`model-auto-btn${mode === "auto" ? " active" : ""}`}
          aria-pressed={mode === "auto"}
          onClick={handleToggleAuto}
          title={
            mode === "auto"
              ? "已开启自动选择：Plan 模式与子代理优先用子模型，Work 模式优先用主模型（点击关闭）"
              : "自动选择：按设置里的主/子模型路由（只对任务会话生效）"
          }
        >
          ⚡ 自动
        </button>
      )}

      {mode === "auto" ? (
        <span className="model-bar-auto-hint" title="在设置页的「模型路由」里配置主模型与子模型">
          {settings?.subs.length
            ? `主：${settings.main?.model ?? "跟随全局"} ｜ 子：${settings.subs
                .map((s) => s.model)
                .join("、")}`
            : "未配置子模型，将使用主模型"}
          {autoEffectiveRef
            ? ` ｜ 本轮：${autoEffectiveRef.model}（${isTaskSession && taskMode === "plan" ? "Plan" : "Work/主"}）`
            : ""}
        </span>
      ) : (
        <select
          className="model-select"
          value={manualValue}
          onChange={(e) => handleModelChange(e.target.value)}
          title="本会话使用的模型（只影响下一轮，不会打断正在生成的回复）"
        >
          <option value={INHERIT_VALUE}>
            跟随全局：{activeProviderModels(catalog)?.current_model || "未配置"}
          </option>
          {/* 会话选定的模型被停用/移出目录后，仍要把当前值显示在下拉框里，
              否则 select 匹配不到 option 会显示空白（用户不知道现在用的是什么） */}
          {mode === "manual" && !effectiveOption && pref?.model && (
            <option value={manualValue}>
              {pref.model}（已停用或不在目录中）
            </option>
          )}
          {catalog.providers.map((provider) => (
            <optgroup
              key={provider.provider_id}
              label={`${provider.provider_name}${provider.is_active ? "（当前）" : ""}${
                provider.is_usable ? "" : " · 未配置密钥"
              }`}
            >
              {provider.models.length === 0 && (
                <option value={`noop-${provider.provider_id}`} disabled>
                  该提供商还没有已启用的模型
                </option>
              )}
              {provider.models.map((model) => (
                <option
                  key={`${provider.provider_id}-${model.id}`}
                  value={modelRefValue({ provider_id: provider.provider_id, model: model.id })}
                >
                  {model.id}
                  {model.supports_thinking ? " · 可思考" : ""}
                </option>
              ))}
            </optgroup>
          ))}
        </select>
      )}

      {showThinkingToggle && (
        <label
          className="model-thinking-toggle"
          title="本会话的深度思考开关：关闭时会向支持该能力的模型显式下发 enable_thinking=false"
        >
          <input
            type="checkbox"
            checked={thinkingChecked}
            onChange={(e) => handleThinkingChange(e.target.checked)}
          />
          深度思考
        </label>
      )}

      {mode === "auto" && autoEffectiveRef?.model && (
        <span className="model-bar-resolved" title="自动选择下本轮实际使用的模型">
          {modelRefLabel(catalog, autoEffectiveRef)}
        </span>
      )}

      <button
        type="button"
        className="model-refresh-btn"
        onClick={() => setCurrentPage("settings")}
        title="下拉框只列出「已在设置里启用」的模型；点这里去设置页拉取并启用更多模型"
      >
        ＋
      </button>
    </div>
  );
}
