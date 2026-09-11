import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useChatStore } from "../../stores/chatStore";

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
}

interface AppConfig {
  user: { nickname: string; pronouns: string; gender: string; birthday: string; bio: string };
  llm: { providers: LlmProvider[]; active_provider_id: string };
  memory: { enabled: boolean; auto_extract: boolean; max_context_memories: number };
  ui: { theme: string; font_size: number };
}

interface ModelInfo {
  id: string;
  owned_by: string | null;
}

export function OnboardingPage() {
  const [step, setStep] = useState(0);
  const [config, setConfig] = useState<AppConfig | null>(null);
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [testResult, setTestResult] = useState("");
  const [testing, setTesting] = useState(false);
  const [fetchingModels, setFetchingModels] = useState(false);
  const [saving, setSaving] = useState(false);
  const setCurrentPage = useChatStore((s) => s.setCurrentPage);

  // 加载默认配置
  useEffect(() => {
    invoke<AppConfig>("get_config").then(setConfig).catch(console.error);
  }, []);

  const activeProvider = config?.llm.providers.find((p) => p.id === config.llm.active_provider_id);

  const updateProvider = (updates: Partial<LlmProvider>) => {
    if (!config || !activeProvider) return;
    const updated = { ...activeProvider, ...updates };
    setConfig({
      ...config,
      llm: {
        ...config.llm,
        providers: config.llm.providers.map((p) => (p.id === updated.id ? updated : p)),
      },
    });
  };

  const handleTestConnection = async () => {
    if (!config || !activeProvider?.model.trim()) return;
    setTesting(true);
    setTestResult("测试中...");
    try {
      await invoke("update_config", { newConfig: config });
      const result = await invoke<string>("test_llm_connection");
      setTestResult(`✅ ${result}`);
    } catch (e) {
      setTestResult(`❌ ${e}`);
    } finally {
      setTesting(false);
    }
  };

  const handleFetchModels = async () => {
    if (!config) return;
    setFetchingModels(true);
    setTestResult("");
    try {
      // 先保存当前配置（含 API URL 和 Key），确保后端使用最新配置
      await invoke("update_config", { newConfig: config });
      const list = await invoke<ModelInfo[]>("fetch_models");
      setModels(list);
      // 自动选中第一个模型
      if (list.length > 0 && !activeProvider?.model.trim()) {
        updateProvider({ model: list[0].id, enabled_models: [list[0].id] });
      }
    } catch (e) {
      setTestResult(`❌ 获取模型列表失败: ${e}`);
    } finally {
      setFetchingModels(false);
    }
  };

  const handleFinish = async () => {
    if (!config) return;
    setSaving(true);
    try {
      await invoke("update_config", { newConfig: config });
      setCurrentPage("chat");
    } catch (e) {
      setTestResult(`❌ 保存失败: ${e}`);
    } finally {
      setSaving(false);
    }
  };

  if (!config || !activeProvider) return <div className="onboarding-page">加载中...</div>;

  return (
    <div className="onboarding-page">
      <div className="onboarding-card">
        {/* Step 0: 欢迎 */}
        {step === 0 && (
          <div className="onboarding-step">
            <div className="onboarding-icon">✦</div>
            <h2>欢迎使用 镜中此方</h2>
            <p className="onboarding-desc">一个基于 LLM 的桌面 AI 聊天助手，支持角色扮演和记忆系统。</p>
            <p className="onboarding-hint">让我们花一分钟完成基础设置。</p>
            <label>
              <span>你希望此方怎么称呼你？</span>
              <input value={config.user.nickname}
                onChange={(e) => setConfig({ ...config, user: { ...config.user, nickname: e.target.value } })}
                placeholder="输入你的昵称" />
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
            <button className="onboarding-next-btn" onClick={() => setStep(1)}>下一步 →</button>
          </div>
        )}

        {/* Step 1: API + 模型配置（合并为一页） */}
        {step === 1 && (
          <div className="onboarding-step">
            <h2>🔗 配置 LLM 服务</h2>
            <p className="onboarding-desc">请填入 API 配置，获取模型列表后选择要使用的模型。</p>
            <label>
              <span>API Base URL</span>
              <input value={activeProvider.api_base_url}
                onChange={(e) => updateProvider({ api_base_url: e.target.value })}
                placeholder="https://api.openai.com/v1" />
            </label>
            <label>
              <span>API Key</span>
              <input type="password" value={activeProvider.api_key}
                onChange={(e) => updateProvider({ api_key: e.target.value })}
                placeholder="sk-..." />
            </label>
            <div className="onboarding-actions">
              <button className="onboarding-fetch-btn" onClick={handleFetchModels}
                disabled={fetchingModels || !activeProvider.api_key || !activeProvider.api_base_url}>
                {fetchingModels ? "获取中..." : "🔄 获取模型列表"}
              </button>
            </div>
            <label>
              <span>模型</span>
              {models.length > 0 ? (
                <select value={activeProvider.model}
                  onChange={(e) => updateProvider({ model: e.target.value, enabled_models: [e.target.value] })}>
                  <option value="" disabled>请选择模型</option>
                  {models.map((m) => <option key={m.id} value={m.id}>{m.id}</option>)}
                </select>
              ) : (
                <input value={activeProvider.model}
                  onChange={(e) => updateProvider({ model: e.target.value, enabled_models: [e.target.value] })}
                  placeholder="请先获取模型列表，或手动输入" />
              )}
            </label>
            <div className="onboarding-actions">
              <button className="onboarding-test-btn" onClick={handleTestConnection}
                disabled={testing || !activeProvider.api_key || !activeProvider.model.trim()}>
                {testing ? "测试中..." : "🔗 测试连接"}
              </button>
            </div>
            {testResult && <div className="onboarding-result">{testResult}</div>}
            <div className="onboarding-nav">
              <button className="onboarding-back-btn" onClick={() => setStep(0)}>← 上一步</button>
              <button className="onboarding-next-btn" onClick={() => setStep(2)}
                disabled={!activeProvider.api_key || !activeProvider.model.trim()}>
                下一步 →
              </button>
            </div>
          </div>
        )}

        {/* Step 2: 完成 */}
        {step === 2 && (
          <div className="onboarding-step">
            <div className="onboarding-icon">✦</div>
            <h2>设置完成！</h2>
            <p className="onboarding-desc">你好，{config.user.nickname}！此方已经准备好和你聊天了。</p>
            <div className="onboarding-summary">
              <div>昵称：{config.user.nickname}</div>
              <div>模型：{activeProvider.model}</div>
              <div>API：{activeProvider.api_base_url}</div>
            </div>
            <div className="onboarding-nav">
              <button className="onboarding-back-btn" onClick={() => setStep(1)}>← 上一步</button>
              <button className="onboarding-finish-btn" onClick={handleFinish} disabled={saving}>
                {saving ? "保存中..." : "✦ 开始聊天"}
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
