use std::fs;
use std::path::{Path, PathBuf};
use tauri::State;

use crate::persona::types::{PersonaConfig, DEFAULT_PERSONA_ID};
use crate::AppState;

/// 人格摘要信息（前端 UI 的唯一来源）
///
/// 除标识信息外还携带**角色外观文案**：短名（按钮/空状态）与戳一戳台词。
/// 这些内容随人格走，因此换人格后按钮文案与桌宠台词会一起变化，
/// 前端不再硬编码任何角色名或台词。
#[derive(serde::Serialize)]
pub struct PersonaSummary {
    pub id: String,
    pub name: String,
    pub version: String,
    pub is_builtin: bool,
    /// 短名，例如「此方」
    pub short_name: String,
    /// 戳一戳预置台词（已过滤空串，必要时为中性兜底）
    pub poke_lines: Vec<String>,
    /// 连续戳一戳到"生气"时的台词
    pub poke_angry_line: String,
}

impl PersonaSummary {
    fn from_config(persona: &PersonaConfig, is_builtin: bool) -> Self {
        Self {
            id: persona.id.clone(),
            name: persona.name.clone(),
            version: persona.version.clone(),
            is_builtin,
            short_name: persona.display_short_name(),
            poke_lines: persona.effective_poke_lines(),
            poke_angry_line: persona.effective_poke_angry_line(),
        }
    }
}

/// 获取人格文件目录路径
pub(crate) fn personas_dir(app_data_dir: &std::path::Path) -> PathBuf {
    app_data_dir.join("personas")
}

/// 校验 persona_id 合法性（防止路径穿越等非法输入）
pub fn validate_persona_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("persona_id 不能为空".to_string());
    }
    if id.chars().count() > 64 {
        return Err("persona_id 长度不能超过 64 个字符".to_string());
    }
    if id.starts_with('.') || id.contains("..") {
        return Err("persona_id 不能以 . 开头或包含 ..".to_string());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err("persona_id 仅允许字母、数字以及 - _ . 字符".to_string());
    }
    Ok(())
}

/// 校验目标文件确实位于指定目录内（路径穿越 / 绝对路径逃逸的双保险）
///
/// 白名单校验已经排除了 `/`、`\`、`:` 与 `..`，这里再对父目录做规范化比对，
/// 防止未来有人放宽白名单时重新打开写入原语。
pub(crate) fn is_inside_dir(dir: &Path, candidate: &Path) -> bool {
    let (Ok(canon_dir), Some(parent)) = (dir.canonicalize(), candidate.parent()) else {
        return false;
    };
    match parent.canonicalize() {
        Ok(canon_parent) => canon_parent == canon_dir,
        Err(_) => false,
    }
}

/// 查找用户人格文件（兼容 .yaml / .yml，优先 .yaml）
fn find_user_persona_file(dir: &Path, persona_id: &str) -> Option<PathBuf> {
    for ext in ["yaml", "yml"] {
        let path = dir.join(format!("{}.{}", persona_id, ext));
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// 将人格 YAML 写入用户目录的唯一入口
///
/// 所有落盘路径都必须经过"id 白名单校验 + 目录包含性校验"：
/// `personas.join(format!("{}.yaml", id))` 在 id 为绝对路径时会被
/// `Path::join` 整体替换（`/home/u/.config/x`），在 id 含 `..` 时可穿越目录，
/// 从而形成任意文件写入原语。
pub(crate) fn write_persona_file(
    dir: &Path,
    persona_id: &str,
    yaml_content: &str,
) -> Result<(), String> {
    validate_persona_id(persona_id)?;

    let file_path = dir.join(format!("{}.yaml", persona_id));
    if !is_inside_dir(dir, &file_path) {
        return Err("非法的人格 ID：写入路径越出人格目录".to_string());
    }

    write_atomic(&file_path, yaml_content)?;

    // 清理可能存在的旧 .yml 文件，避免双文件遮蔽混乱
    let legacy_path = dir.join(format!("{}.yml", persona_id));
    if legacy_path.exists() {
        let _ = fs::remove_file(&legacy_path);
    }

    Ok(())
}

/// 原子写入：临时文件 → fsync → rename
///
/// 人格文件每次对话都要读取，写一半崩溃会留下坏 YAML；与 `config.json`
/// 使用同一套"要么旧内容、要么完整新内容"的写法。
fn write_atomic(path: &Path, content: &str) -> Result<(), String> {
    use std::io::Write;

    let dir = path
        .parent()
        .ok_or_else(|| "无法确定人格文件目录".to_string())?;
    let tmp = dir.join(format!(".persona-tmp-{}", uuid::Uuid::new_v4()));
    {
        let mut file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        if let Err(e) = file.write_all(content.as_bytes()).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&tmp);
            return Err(e.to_string());
        }
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}

/// 获取所有可用人格列表（同 ID 去重：用户文件覆盖内置，与运行时引擎语义一致）
#[tauri::command]
pub async fn list_personas(state: State<'_, AppState>) -> Result<Vec<PersonaSummary>, String> {
    let mut personas = Vec::new();

    // 1. 先收集用户自定义人格（app_data_dir/personas/），按 id 去重
    let dir = personas_dir(&state.app_data_dir);
    let mut user_by_id: std::collections::HashMap<String, PersonaSummary> =
        std::collections::HashMap::new();
    if dir.exists() {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "yaml" || e == "yml") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        if let Ok(p) = serde_yaml::from_str::<PersonaConfig>(&content) {
                            // 同 ID 多个文件时保留首个（与引擎加载顺序一致）
                            user_by_id
                                .entry(p.id.clone())
                                .or_insert_with(|| PersonaSummary::from_config(&p, false));
                        }
                    }
                }
            }
        }
    }

    // 2. 内置人格：若已被用户同名 ID 覆盖，则只展示覆盖后的版本（运行时实际生效的）
    let builtin_yaml = include_str!("../../personas/default.yaml");
    if let Ok(p) = serde_yaml::from_str::<PersonaConfig>(builtin_yaml) {
        if !user_by_id.contains_key(&p.id) {
            personas.push(PersonaSummary::from_config(&p, true));
        }
    }

    // 3. 用户人格按名称排序保证列表顺序稳定（read_dir 顺序不确定）
    let mut user_list: Vec<PersonaSummary> = user_by_id.into_values().collect();
    user_list.sort_by(|a, b| a.name.cmp(&b.name));
    personas.extend(user_list);

    Ok(personas)
}

/// 获取指定人格的完整 YAML 内容
#[tauri::command]
pub async fn get_persona_yaml(
    state: State<'_, AppState>,
    persona_id: String,
) -> Result<String, String> {
    validate_persona_id(&persona_id)?;

    // 1. 先查用户文件（兼容 .yaml / .yml）
    let dir = personas_dir(&state.app_data_dir);
    if let Some(user_file) = find_user_persona_file(&dir, &persona_id) {
        return fs::read_to_string(&user_file).map_err(|e| e.to_string());
    }

    // 2. 再查内置
    let builtin_yaml = include_str!("../../personas/default.yaml");
    if let Ok(p) = serde_yaml::from_str::<PersonaConfig>(builtin_yaml) {
        if p.id == persona_id {
            return Ok(builtin_yaml.to_string());
        }
    }

    Err(format!("Persona '{}' not found", persona_id))
}

/// 解析某个人格（缺省为当前"今日会话"的人格）的 UI 文案
///
/// 悬浮窗常驻且不加载会话列表，因此由后端统一解析：
/// 显式 id → 今日会话的人格 → 内置默认人格。
#[tauri::command]
pub async fn get_persona_summary(
    state: State<'_, AppState>,
    persona_id: Option<String>,
) -> Result<PersonaSummary, String> {
    let resolved_id = match persona_id
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
    {
        Some(id) => id,
        None => {
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let store = state.chat_store.lock().map_err(|e| e.to_string())?;
            store
                .find_session_by_date(&today, "chat")
                .ok()
                .flatten()
                .or_else(|| store.find_latest_empty_session("chat").ok().flatten())
                .map(|session| session.persona_id)
                .unwrap_or_else(|| DEFAULT_PERSONA_ID.to_string())
        }
    };

    let is_builtin = {
        !personas_dir(&state.app_data_dir)
            .join(format!("{}.yaml", resolved_id))
            .exists()
    };

    let engine = state.personas.read().map_err(|e| e.to_string())?;
    let persona = engine
        .get_persona(&resolved_id)
        .or_else(|| engine.default_persona())
        .ok_or_else(|| "没有可用人格".to_string())?;

    Ok(PersonaSummary::from_config(persona, is_builtin))
}

/// 保存人格 YAML 文件
#[tauri::command]
pub async fn save_persona(
    state: State<'_, AppState>,
    persona_id: String,
    yaml_content: String,
) -> Result<(), String> {
    validate_persona_id(&persona_id)?;

    // 验证 YAML 格式
    let parsed: PersonaConfig =
        serde_yaml::from_str(&yaml_content).map_err(|e| format!("YAML 解析错误: {}", e))?;

    // 校验 YAML 内部 id 与目标 ID 一致，避免「按 A 文件名保存、以 B id 索引」导致
    // 会话引用旧 ID 时静默回退默认人格
    if parsed.id != persona_id {
        return Err(format!(
            "YAML 中的 id \"{}\" 与目标 ID \"{}\" 不一致，请修改后重试",
            parsed.id, persona_id
        ));
    }

    // 保存到用户目录（统一使用 .yaml 扩展名）
    let dir = personas_dir(&state.app_data_dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    write_persona_file(&dir, &persona_id, &yaml_content)?;

    // 热重载：保存的人格立即对聊天生效（无需重启）
    state.dispatcher.chat_agent().reload_personas(&dir);

    Ok(())
}

/// 删除用户自定义人格
#[tauri::command]
pub async fn delete_persona(
    state: State<'_, AppState>,
    persona_id: String,
) -> Result<(), String> {
    validate_persona_id(&persona_id)?;

    let dir = personas_dir(&state.app_data_dir);

    if let Some(file_path) = find_user_persona_file(&dir, &persona_id) {
        fs::remove_file(&file_path).map_err(|e| e.to_string())?;
        // 热重载：删除的人格立即从聊天中移除
        state.dispatcher.chat_agent().reload_personas(&state.app_data_dir);
        Ok(())
    } else if persona_id == DEFAULT_PERSONA_ID {
        Err("该人格为内置人格，无法删除".to_string())
    } else {
        Err("未找到该人格的用户文件".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_persona_id_accepts_normal_ids() {
        assert!(validate_persona_id("konata-default").is_ok());
        assert!(validate_persona_id("custom-1700000000000").is_ok());
        assert!(validate_persona_id("My_Char.v2").is_ok());
    }

    #[test]
    fn validate_persona_id_rejects_traversal_and_invalid() {
        assert!(validate_persona_id("").is_err());
        assert!(validate_persona_id("../etc/passwd").is_err()); // 含 / 且 ..
        assert!(validate_persona_id("..").is_err());
        assert!(validate_persona_id(".hidden").is_err());
        assert!(validate_persona_id("a\\..\\b").is_err()); // 反斜杠非法
        assert!(validate_persona_id("id with space").is_err());
        assert!(validate_persona_id("中文id").is_err());
        assert!(validate_persona_id("/etc/cron.d/evil").is_err()); // 绝对路径
        assert!(validate_persona_id("C:\\Users\\me\\x").is_err()); // Windows 绝对路径
        assert!(validate_persona_id(&"a".repeat(65)).is_err()); // 超长
    }

    #[test]
    fn is_inside_dir_rejects_escapes() {
        let root = std::env::temp_dir().join(format!("konata-inside-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("personas");
        fs::create_dir_all(&dir).unwrap();

        assert!(is_inside_dir(&dir, &dir.join("ok.yaml")));
        assert!(!is_inside_dir(&dir, &dir.join("../escaped.yaml")));
        assert!(!is_inside_dir(&dir, Path::new("/tmp/absolutely-outside.yaml")));
        assert!(!is_inside_dir(&dir, &root.join("sibling.yaml")));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_persona_file_blocks_path_traversal() {
        let root = std::env::temp_dir().join(format!("konata-write-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("personas");
        fs::create_dir_all(&dir).unwrap();

        // 正常写入
        write_persona_file(&dir, "safe-id", "id: safe-id\n").unwrap();
        assert!(dir.join("safe-id.yaml").exists());

        // 目录穿越与绝对路径都必须被拒绝，且不在目标位置留下文件
        let outside = root.join("evil.yaml");
        assert!(write_persona_file(&dir, "../evil", "payload").is_err());
        assert!(write_persona_file(&dir, "/tmp/konata-should-not-exist", "payload").is_err());
        assert!(!outside.exists());
        assert!(!Path::new("/tmp/konata-should-not-exist.yaml").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_user_persona_file_prefers_yaml_over_yml() {
        let root = std::env::temp_dir().join(format!("konata-find-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        assert!(find_user_persona_file(&root, "missing").is_none());

        fs::write(root.join("a.yml"), "x").unwrap();
        assert_eq!(
            find_user_persona_file(&root, "a").unwrap(),
            root.join("a.yml")
        );

        fs::write(root.join("b.yaml"), "x").unwrap();
        fs::write(root.join("b.yml"), "x").unwrap();
        assert_eq!(
            find_user_persona_file(&root, "b").unwrap(),
            root.join("b.yaml"),
            "同名时优先 .yaml"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
