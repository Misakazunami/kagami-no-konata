use serde::Serialize;
use tauri::{Emitter, State};

use crate::config::types::{AppConfig, LlmProvider};
use crate::llm::types::ModelInfo;
use crate::AppState;

/// 错误信息中透传给前端的响应体长度上限（避免把上游整段 body 灌进 UI）
const MAX_ERROR_BODY_CHARS: usize = 200;

/// 按字符（而非字节）截断字符串，避免 `&s[..n]` 落在多字节字符中间导致 panic
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// 在配置副本上应用修改：校验 → 落盘成功 → 才提交到内存并热更新 dispatcher
///
/// 统一了此前各命令不一致的顺序（部分命令"先改内存、后落盘"，
/// 落盘失败时会留下内存与 config.json 分叉的运行时状态）。
fn mutate_config<T, F>(state: &AppState, apply: F) -> Result<T, String>
where
    F: FnOnce(&mut AppConfig) -> Result<T, String>,
{
    // 只拿 config 一把锁。数据目录已改为不可变字段（无需锁），历史实现
    // 先锁 data_dir 再锁 config，与其它命令的相反顺序构成 ABBA 死锁。
    let mut config = state.config.lock().map_err(|e| e.to_string())?;

    let mut draft = config.clone();
    let output = apply(&mut draft)?;
    draft.validate()?;
    // 持锁落盘是有意为之：它保证"读-改-写"整体原子，并发修改不会互相覆盖。
    // 因为没有第二把锁，这里的 I/O 只可能阻塞、不可能死锁。
    crate::config::save_config(&state.app_data_dir, &draft)
        .map_err(|e| format!("配置保存失败: {}", e))?;

    let provider = draft.llm.active_provider().clone();
    *config = draft;
    drop(config);

    state.dispatcher.chat_agent().update_provider(&provider);
    Ok(output)
}

/// 获取当前配置
#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> Result<AppConfig, String> {
    let config = state.config.lock().map_err(|e| e.to_string())?;
    Ok(config.clone())
}

/// 更新配置
#[tauri::command]
pub async fn update_config(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    new_config: AppConfig,
) -> Result<(), String> {
    // 注意：这里不接受空 providers（会写入自毁式配置），由 validate 明确报错，
    // 而不是静默修复 —— 静默修复会掩盖前端的状态错误。
    mutate_config(&state, |config| {
        *config = new_config;
        Ok(())
    })?;

    let _ = app.emit("config-updated", ());
    Ok(())
}

/// 测试指定提供商的连接
#[tauri::command]
pub async fn test_provider_connection(
    state: State<'_, AppState>,
    provider_id: Option<String>,
) -> Result<String, String> {
    let provider = {
        let config = state.config.lock().map_err(|e| e.to_string())?;
        match provider_id {
            Some(id) => config
                .llm
                .providers
                .iter()
                .find(|p| p.id == id)
                .cloned()
                .ok_or("提供商不存在")?,
            None => config.llm.active_provider().clone(),
        }
    };

    if provider.api_base_url.trim().is_empty() {
        return Err("请先填写 API 地址".to_string());
    }
    if provider.model.trim().is_empty() {
        return Err("请先选择或填写模型".to_string());
    }

    let proxy = crate::llm::proxy::LlmProxy::new(&provider);
    let test_msg = vec![crate::llm::types::LlmMessage::user("Hi")];
    match proxy.chat(test_msg).await {
        // 按字符截断：`&response[..100]` 会在中文回复上切到多字节边界并直接 panic
        Ok(response) => Ok(format!("连接成功！模型响应: {}", truncate_chars(&response, 100))),
        Err(e) => Err(format!(
            "连接失败: {}",
            truncate_chars(&e.to_string(), MAX_ERROR_BODY_CHARS)
        )),
    }
}

/// 获取指定提供商的模型列表
#[tauri::command]
pub async fn fetch_provider_models(
    state: State<'_, AppState>,
    provider_id: Option<String>,
) -> Result<Vec<ModelInfo>, String> {
    let provider = {
        let config = state.config.lock().map_err(|e| e.to_string())?;
        match provider_id {
            Some(id) => config
                .llm
                .providers
                .iter()
                .find(|p| p.id == id)
                .cloned()
                .ok_or("提供商不存在")?,
            None => config.llm.active_provider().clone(),
        }
    };

    if provider.api_base_url.trim().is_empty() {
        return Err("请先填写 API 地址".to_string());
    }

    let proxy = crate::llm::proxy::LlmProxy::new(&provider);
    proxy.list_models().await.map_err(|e| e.to_string())
}

/// 添加提供商
#[tauri::command]
pub async fn add_provider(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    name: String,
    api_base_url: String,
    api_key: String,
) -> Result<String, String> {
    let name = name.trim().to_string();
    let api_base_url = api_base_url.trim().to_string();
    if name.is_empty() {
        return Err("提供商名称不能为空".to_string());
    }
    if api_base_url.is_empty() {
        return Err("API 地址不能为空".to_string());
    }
    if !(api_base_url.starts_with("http://") || api_base_url.starts_with("https://")) {
        return Err("API 地址必须以 http:// 或 https:// 开头".to_string());
    }
    if name.chars().count() > 64 {
        return Err("提供商名称过长".to_string());
    }

    let provider = LlmProvider::new(&name, &api_base_url, api_key.trim());
    let id = provider.id.clone();
    let id_in_closure = id.clone();

    mutate_config(&state, move |config| {
        config.llm.add_provider(provider);
        Ok(id_in_closure)
    })?;

    let _ = app.emit("config-updated", ());
    Ok(id)
}

/// 更新提供商
#[tauri::command]
pub async fn update_provider(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    provider_id: String,
    provider: LlmProvider,
) -> Result<(), String> {
    mutate_config(&state, move |config| {
        let existing = config
            .llm
            .providers
            .iter_mut()
            .find(|p| p.id == provider_id)
            .ok_or("提供商不存在")?;

        // 强制沿用路径参数里的 id：允许请求体改写 id 会让 active_provider_id 悬空，
        // 进而静默回退到"列表第一个提供商"，用户以为在用 A 而请求实际打到 B。
        let mut updated = provider;
        updated.id = provider_id.clone();
        *existing = updated;
        Ok(())
    })?;

    let _ = app.emit("config-updated", ());
    Ok(())
}

/// 删除提供商
#[tauri::command]
pub async fn delete_provider(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    provider_id: String,
) -> Result<(), String> {
    mutate_config(&state, move |config| config.llm.remove_provider(&provider_id))?;

    let _ = app.emit("config-updated", ());
    Ok(())
}

/// 切换活跃提供商
#[tauri::command]
pub async fn set_active_provider(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    provider_id: String,
) -> Result<(), String> {
    mutate_config(&state, move |config| config.llm.set_active(&provider_id))?;

    let _ = app.emit("config-updated", ());
    Ok(())
}

/// 设置活跃提供商的当前模型
#[tauri::command]
pub async fn set_active_model(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    model_id: String,
) -> Result<(), String> {
    let model_id = model_id.trim().to_string();
    if model_id.is_empty() {
        return Err("模型 ID 不能为空".to_string());
    }

    mutate_config(&state, move |config| {
        let provider = config
            .llm
            .active_provider_mut()
            .ok_or("当前没有可用的提供商")?;
        provider.model = model_id.clone();
        if !provider.enabled_models.contains(&model_id) {
            provider.enabled_models.push(model_id);
        }
        Ok(())
    })?;

    let _ = app.emit("config-updated", ());
    Ok(())
}

/// 以下是向后兼容的旧命令（转发到新接口）

#[tauri::command]
pub async fn test_llm_connection(state: State<'_, AppState>) -> Result<String, String> {
    test_provider_connection(state, None).await
}

#[tauri::command]
pub async fn fetch_models(state: State<'_, AppState>) -> Result<Vec<ModelInfo>, String> {
    fetch_provider_models(state, None).await
}

// ─── 模型目录（对话界面选择器 / 设置页模型路由共用） ───────

/// 一个可选模型（含能力探测结果）
#[derive(Debug, Clone, Serialize)]
pub struct ModelOption {
    pub id: String,
    /// 是否支持深度思考（界面据此决定要不要显示"深度思考"开关）
    pub supports_thinking: bool,
    /// 是否是当前选中的模型
    pub is_current: bool,
}

/// 一个提供商下的可选模型
#[derive(Debug, Clone, Serialize)]
pub struct ProviderModels {
    pub provider_id: String,
    pub provider_name: String,
    pub is_active: bool,
    pub is_usable: bool,
    pub current_model: String,
    /// 该提供商在设置里声明的"默认开启思考"（会话未单独设置时的实际取值）
    pub thinking_default: bool,
    pub models: Vec<ModelOption>,
}

/// 模型目录
#[derive(Debug, Clone, Serialize)]
pub struct ModelCatalog {
    pub providers: Vec<ProviderModels>,
    /// 自动选择的主/子模型池配置
    pub settings: crate::config::types::ModelSettings,
}

/// 列出可选择的模型
///
/// 只列**已启用**的模型（`enabled_models` + 当前模型），而不是把上游
/// `/models` 返回的几百个模型全灌进界面 —— 想要更多模型请先去设置页启用。
/// 能力探测在 Rust 侧完成，界面不重复实现一套启发式。
#[tauri::command]
pub async fn get_model_catalog(state: State<'_, AppState>) -> Result<ModelCatalog, String> {
    let config = state.config.lock().map_err(|e| e.to_string())?;

    let providers = config
        .llm
        .providers
        .iter()
        .map(|p| {
            // 当前模型始终在列表里：否则用户会看到"当前模型不在下拉框里"
            let mut ids = p.enabled_models.clone();
            if !p.model.trim().is_empty() && !ids.contains(&p.model) {
                ids.insert(0, p.model.clone());
            }

            ProviderModels {
                provider_id: p.id.clone(),
                provider_name: if p.name.trim().is_empty() {
                    p.id.clone()
                } else {
                    p.name.clone()
                },
                is_active: p.id == config.llm.active_provider_id,
                is_usable: !p.api_base_url.trim().is_empty() && !p.api_key.trim().is_empty(),
                current_model: p.model.clone(),
                thinking_default: p.enable_thinking,
                models: ids
                    .into_iter()
                    .map(|id| ModelOption {
                        supports_thinking: crate::llm::capabilities::provider_supports_thinking(p, &id),
                        is_current: id == p.model,
                        id,
                    })
                    .collect(),
            }
        })
        .collect();

    Ok(ModelCatalog {
        providers,
        settings: config.models.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_is_multibyte_safe() {
        let chinese = "此方".repeat(100); // 200 个字符 / 600 字节
        let out = truncate_chars(&chinese, 100);
        assert_eq!(out.chars().count(), 101); // 100 字符 + 省略号
        assert!(out.starts_with('此'));

        // 历史实现 &s[..100] 会在此处 panic（第 100 字节落在字符中间）
        assert_eq!(chinese.len().min(100), 100);
    }

    #[test]
    fn truncate_chars_keeps_short_text_intact() {
        assert_eq!(truncate_chars("你好", 100), "你好");
        assert_eq!(truncate_chars("", 10), "");
    }
}
