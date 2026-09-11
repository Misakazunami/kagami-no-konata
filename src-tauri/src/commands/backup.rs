use std::fs;
use tauri::State;

use crate::commands::persona::{is_inside_dir, personas_dir, validate_persona_id, write_persona_file};
use crate::persona::types::PersonaConfig;
use crate::store::memory_store::{MemoryEntry, MemoryStore, MAX_IMPORT_ENTRIES};
use crate::AppState;

/// 导入 JSON 的大小上限（防止一次 IPC 把进程内存打满）
const MAX_IMPORT_BYTES: usize = 64 * 1024 * 1024;
/// 人格备份中单份 YAML 的长度上限
const MAX_PERSONA_YAML_BYTES: usize = 256 * 1024;

/// 记忆备份数据
#[derive(serde::Serialize, serde::Deserialize)]
pub struct MemoryBackup {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub exported_at: String,
    pub memories: Vec<MemoryEntry>,
}

/// 人格备份数据
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PersonaBackup {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub exported_at: String,
    pub personas: Vec<PersonaBackupEntry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct PersonaBackupEntry {
    pub id: String,
    pub name: String,
    /// 是否为内置人格（导入时跳过，避免用户目录副本永久遮蔽后续版本更新）
    #[serde(default)]
    pub is_builtin: bool,
    pub yaml_content: String,
}

/// 导入结果（让界面能如实展示"成功/跳过/失败"，而不是把尝试数当成功数）
#[derive(serde::Serialize)]
pub struct ImportReport {
    pub imported: usize,
    pub skipped: usize,
    pub failed: usize,
    /// 因安全策略被拒绝的条目数（例如试图顶替内置人格）
    pub blocked: usize,
    /// 导入后仍缺少向量、暂时无法被检索到的条目数
    pub without_embedding: usize,
}

impl ImportReport {
    fn new() -> Self {
        Self {
            imported: 0,
            skipped: 0,
            failed: 0,
            blocked: 0,
            without_embedding: 0,
        }
    }
}

/// 校验并解析导入用的 JSON 文本
fn parse_import<T: serde::de::DeserializeOwned>(json_content: &str, what: &str) -> Result<T, String> {
    if json_content.len() > MAX_IMPORT_BYTES {
        return Err(format!(
            "导入文件过大（{} 字节，上限 {} 字节）",
            json_content.len(),
            MAX_IMPORT_BYTES
        ));
    }
    serde_json::from_str(json_content).map_err(|e| format!("{}解析失败: {}", what, e))
}

/// 导出所有记忆到 JSON 字符串
#[tauri::command]
pub async fn export_memories(state: State<'_, AppState>) -> Result<String, String> {
    let store = state.memory_store.lock().map_err(|e| e.to_string())?;
    let memories = store.list_memories(None).map_err(|e| e.to_string())?;

    let backup = MemoryBackup {
        version: 1,
        exported_at: chrono::Local::now().to_rfc3339(),
        memories,
    };

    serde_json::to_string_pretty(&backup).map_err(|e| e.to_string())
}

/// 从 JSON 字符串导入记忆
///
/// 与历史实现的区别：
/// - 清空 + 写入在**同一事务**内完成，任一步失败整体回滚（不再"清空成功、写入失败"）
/// - 逐条错误不再被 `let _ =` 吞掉，返回值区分成功/跳过/失败
/// - 外部条目先经 `sanitize_imported` 清洗（长度、importance 越界、向量维度）
#[tauri::command]
pub async fn import_memories(
    state: State<'_, AppState>,
    json_content: String,
    merge: bool,
) -> Result<ImportReport, String> {
    let backup: MemoryBackup = parse_import(&json_content, "记忆备份")?;

    if backup.memories.len() > MAX_IMPORT_ENTRIES {
        return Err(format!(
            "备份包含 {} 条记忆，超过单次导入上限 {} 条",
            backup.memories.len(),
            MAX_IMPORT_ENTRIES
        ));
    }

    let mut report = ImportReport::new();
    let mut entries = Vec::with_capacity(backup.memories.len());
    for entry in backup.memories {
        match MemoryStore::sanitize_imported(entry) {
            Some(clean) => {
                if clean.embedding.is_none() {
                    report.without_embedding += 1;
                }
                entries.push(clean);
            }
            None => report.skipped += 1,
        }
    }

    let store = state.memory_store.lock().map_err(|e| e.to_string())?;
    let (imported, failed) = store
        .import_memories(&entries, merge)
        .map_err(|e| format!("导入失败（已回滚，未改动现有数据）: {}", e))?;

    report.imported = imported;
    report.failed = failed;
    Ok(report)
}

/// 导出所有人格到 JSON 字符串
#[tauri::command]
pub async fn export_personas(state: State<'_, AppState>) -> Result<String, String> {
    let data_dir = state.app_data_dir.lock().map_err(|e| e.to_string())?;
    let dir = personas_dir(&data_dir);
    let mut entries = Vec::new();

    // 内置人格（标记 is_builtin，导入端会跳过）
    let builtin_yaml = include_str!("../../personas/default.yaml");
    if let Ok(p) = serde_yaml::from_str::<PersonaConfig>(builtin_yaml) {
        entries.push(PersonaBackupEntry {
            id: p.id,
            name: p.name,
            is_builtin: true,
            yaml_content: builtin_yaml.to_string(),
        });
    }

    // 用户自定义人格
    if dir.exists() {
        if let Ok(dir_entries) = fs::read_dir(&dir) {
            for entry in dir_entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "yaml" || e == "yml") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        if let Ok(p) = serde_yaml::from_str::<PersonaConfig>(&content) {
                            entries.push(PersonaBackupEntry {
                                id: p.id,
                                name: p.name,
                                is_builtin: false,
                                yaml_content: content,
                            });
                        }
                    }
                }
            }
        }
    }

    let backup = PersonaBackup {
        version: 1,
        exported_at: chrono::Local::now().to_rfc3339(),
        personas: entries,
    };

    serde_json::to_string_pretty(&backup).map_err(|e| e.to_string())
}

/// 从 JSON 字符串导入人格
///
/// `entry.id` 直接参与文件路径拼接，因此必须经过与 `save_persona` 完全相同的
/// 白名单校验（历史实现漏掉了这一步，形成任意文件写入原语）：
/// - `id = "../../x"` 目录穿越
/// - `id = "/home/u/.config/x"` 绝对路径（`Path::join` 会整体替换基路径）
#[tauri::command]
pub async fn import_personas(
    state: State<'_, AppState>,
    json_content: String,
) -> Result<ImportReport, String> {
    let backup: PersonaBackup = parse_import(&json_content, "人格备份")?;

    let mut report = ImportReport::new();

    if backup.personas.len() > MAX_IMPORT_ENTRIES {
        return Err(format!(
            "备份包含 {} 个人格，超过单次导入上限 {} 个",
            backup.personas.len(),
            MAX_IMPORT_ENTRIES
        ));
    }

    let data_dir = state.app_data_dir.lock().map_err(|e| e.to_string())?;
    let dir = personas_dir(&data_dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    // 规范化一次目录，供包含性校验复用
    let canonical_dir = dir.canonicalize().map_err(|e| e.to_string())?;

    for entry in &backup.personas {
        // 内置人格嵌入在二进制中，导入用户副本只会永久遮蔽后续版本更新
        if entry.is_builtin {
            report.skipped += 1;
            continue;
        }

        // `is_builtin` 是备份文件自己声明的字段，不可信：
        // 把它设为 false 并写入 id = konata-default，即可静默顶替内置人格的
        // system prompt（位于最高权限的系统通道，且会持久化到每次对话）。
        // 用户仍可通过人格编辑器显式覆盖内置人格，但"导入即覆盖"必须被拒绝。
        if entry.id == crate::persona::types::DEFAULT_PERSONA_ID {
            eprintln!("[persona] 已拒绝导入：试图覆盖内置人格 {}", entry.id);
            report.blocked += 1;
            continue;
        }

        if entry.yaml_content.len() > MAX_PERSONA_YAML_BYTES {
            report.skipped += 1;
            continue;
        }

        if validate_persona_id(&entry.id).is_err() {
            report.skipped += 1;
            continue;
        }

        let parsed = match serde_yaml::from_str::<PersonaConfig>(&entry.yaml_content) {
            Ok(parsed) => parsed,
            Err(_) => {
                report.skipped += 1;
                continue;
            }
        };

        // YAML 内部 id 必须与文件名 id 一致，否则会"按 A 文件名保存、以 B id 索引"
        if parsed.id != entry.id {
            report.skipped += 1;
            continue;
        }

        // 双保险：解析后的真实路径必须仍位于人格目录内
        let target = canonical_dir.join(format!("{}.yaml", entry.id));
        if !is_inside_dir(&canonical_dir, &target) {
            report.skipped += 1;
            continue;
        }

        match write_persona_file(&canonical_dir, &entry.id, &entry.yaml_content) {
            Ok(()) => report.imported += 1,
            Err(e) => {
                eprintln!("[persona] 导入失败 {}: {}", entry.id, e);
                report.failed += 1;
            }
        }
    }

    // 热重载：导入的人格立即生效（与保存/删除行为一致）
    state.dispatcher.chat_agent().reload_personas(&data_dir);

    Ok(report)
}
