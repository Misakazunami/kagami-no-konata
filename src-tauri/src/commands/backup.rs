use std::fs;
use tauri::State;

use crate::commands::persona::{is_inside_dir, personas_dir, validate_persona_id, write_persona_file};
use crate::persona::types::PersonaConfig;
use crate::store::memory_store::{
    decode_embedding_b64, encode_embedding_b64, MemoryEntry, MemoryStore, MemoryType,
    MAX_IMPORT_ENTRIES,
};
use crate::AppState;

/// 导入 JSON 的大小上限（防止一次 IPC 把进程内存打满）
const MAX_IMPORT_BYTES: usize = 64 * 1024 * 1024;
/// 人格备份中单份 YAML 的长度上限
const MAX_PERSONA_YAML_BYTES: usize = 256 * 1024;
/// 导出 JSON 的软上限：超过就自动丢弃向量（并标记），避免把几百 MB 的
/// 字符串灌进 IPC 与前端 Blob，最后还超过导入上限、备份变得不可恢复
const MAX_EXPORT_BYTES: usize = 24 * 1024 * 1024;

/// 备份里的一条记忆
///
/// `embedding_b64` 是新格式（f32 小端 + Base64，约为 JSON 数字数组的一半）；
/// `embedding` 字段保留用于导入旧备份。导出时只写 `embedding_b64`。
#[derive(serde::Serialize, serde::Deserialize)]
pub struct MemoryBackupEntry {
    pub id: String,
    pub content: String,
    #[serde(default)]
    pub memory_type: MemoryType,
    #[serde(default = "default_importance")]
    pub importance: f32,
    #[serde(default)]
    pub source_session: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub last_accessed: String,
    #[serde(default)]
    pub access_count: i32,
    /// 旧格式：JSON 数字数组（仅导入时识别）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    /// 新格式：f32 小端 + Base64
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_b64: Option<String>,
}

fn default_importance() -> f32 {
    0.5
}

impl MemoryBackupEntry {
    fn from_entry(memory: &MemoryEntry, include_embedding: bool) -> Self {
        Self {
            id: memory.id.clone(),
            content: memory.content.clone(),
            memory_type: memory.memory_type.clone(),
            importance: memory.importance,
            source_session: memory.source_session.clone(),
            created_at: memory.created_at.clone(),
            last_accessed: memory.last_accessed.clone(),
            access_count: memory.access_count,
            embedding: None,
            embedding_b64: if include_embedding {
                memory.embedding.as_deref().map(encode_embedding_b64)
            } else {
                None
            },
        }
    }

    fn into_entry(self) -> MemoryEntry {
        // 优先新格式；旧备份的 JSON 数组继续兼容
        let embedding = self
            .embedding_b64
            .as_deref()
            .and_then(decode_embedding_b64)
            .or(self.embedding);
        MemoryEntry {
            id: self.id,
            content: self.content,
            memory_type: self.memory_type,
            importance: self.importance,
            embedding,
            source_session: self.source_session,
            created_at: self.created_at,
            last_accessed: self.last_accessed,
            access_count: self.access_count,
        }
    }
}

/// 记忆备份数据
#[derive(serde::Serialize, serde::Deserialize)]
pub struct MemoryBackup {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub exported_at: String,
    /// 因体积超限而**主动丢弃**了向量（导入后这些记忆仍可被按重要性检索到，
    /// 只是语义相似度排序会弱一些；如实在备份里标记，用户才能知情）
    #[serde(default)]
    pub embeddings_omitted: bool,
    pub memories: Vec<MemoryBackupEntry>,
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
///
/// `include_embeddings` 默认 true：向量以 Base64 紧凑编码写入，保证导出的
/// 备份能原样导回、语义检索能力不丢。若编码后超过 [`MAX_EXPORT_BYTES`]，
/// 会自动改为不带向量导出并设置 `embeddings_omitted`（宁可丢排序精度，
/// 也不能产出根本导不回来的备份）。
#[tauri::command]
pub async fn export_memories(
    state: State<'_, AppState>,
    include_embeddings: Option<bool>,
) -> Result<String, String> {
    let include_embeddings = include_embeddings.unwrap_or(true);
    // 全量读取 + 向量编码 + JSON 序列化都是 CPU/IO 重活：放到阻塞线程
    let store = state.memory_store.clone();
    tokio::task::spawn_blocking(move || -> Result<String, String> {
        let memories = {
            let store = store.lock().map_err(|e| e.to_string())?;
            store.list_memories(None).map_err(|e| e.to_string())?
        };

        let build = |include: bool, omitted: bool| MemoryBackup {
            version: 2,
            exported_at: chrono::Local::now().to_rfc3339(),
            embeddings_omitted: omitted,
            memories: memories
                .iter()
                .map(|memory| MemoryBackupEntry::from_entry(memory, include))
                .collect(),
        };

        let mut backup = build(include_embeddings, false);
        let mut json = serde_json::to_string_pretty(&backup).map_err(|e| e.to_string())?;

        if include_embeddings && json.len() > MAX_EXPORT_BYTES {
            eprintln!(
                "[memory] 导出 {} 条记忆含向量约 {} MB，超过 {} MB 上限：改为不含向量导出",
                memories.len(),
                json.len() / 1024 / 1024,
                MAX_EXPORT_BYTES / 1024 / 1024
            );
            backup = build(false, true);
            json = serde_json::to_string_pretty(&backup).map_err(|e| e.to_string())?;
        }

        Ok(json)
    })
    .await
    .map_err(|e| format!("导出任务失败：{}", e))?
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
    // 解析（可能 64 MB JSON）、清洗与 5 万条事务写入都是重活：整体放到阻塞线程
    let store = state.memory_store.clone();
    tokio::task::spawn_blocking(move || -> Result<ImportReport, String> {
        let backup: MemoryBackup = parse_import(&json_content, "记忆备份")?;

        if backup.memories.len() > MAX_IMPORT_ENTRIES {
            return Err(format!(
                "备份包含 {} 条记忆，超过单次导入上限 {} 条",
                backup.memories.len(),
                MAX_IMPORT_ENTRIES
            ));
        }
        if backup.embeddings_omitted {
            eprintln!("[memory] 该备份导出时未包含向量：导入后按重要性参与检索");
        }

        let mut report = ImportReport::new();
        let mut entries = Vec::with_capacity(backup.memories.len());
        for entry in backup.memories {
            match MemoryStore::sanitize_imported(entry.into_entry()) {
                Some(clean) => {
                    if clean.embedding.is_none() {
                        report.without_embedding += 1;
                    }
                    entries.push(clean);
                }
                None => report.skipped += 1,
            }
        }

        let store = store.lock().map_err(|e| e.to_string())?;
        let (imported, failed) = store
            .import_memories(&entries, merge)
            .map_err(|e| format!("导入失败（已回滚，未改动现有数据）: {}", e))?;

        report.imported = imported;
        report.failed = failed;
        Ok(report)
    })
    .await
    .map_err(|e| format!("导入任务失败：{}", e))?
}

/// 导出所有人格到 JSON 字符串
#[tauri::command]
pub async fn export_personas(state: State<'_, AppState>) -> Result<String, String> {
    let dir = personas_dir(&state.app_data_dir);
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

    let data_dir = state.app_data_dir.clone();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> MemoryEntry {
        MemoryEntry {
            id: "m1".to_string(),
            content: "用户喜欢猫".to_string(),
            memory_type: MemoryType::Preference,
            importance: 0.8,
            embedding: Some(vec![0.1, -0.25, 0.75, 1.0]),
            source_session: "s1".to_string(),
            created_at: "2026-01-01T00:00:00+00:00".to_string(),
            last_accessed: "2026-01-01T00:00:00+00:00".to_string(),
            access_count: 3,
        }
    }

    /// 新格式备份必须无损往返（含向量），且不能出现 JSON 数字数组
    #[test]
    fn backup_round_trips_embeddings_compactly() {
        let backup = MemoryBackup {
            version: 2,
            exported_at: "now".to_string(),
            embeddings_omitted: false,
            memories: vec![MemoryBackupEntry::from_entry(&entry(), true)],
        };
        let json = serde_json::to_string(&backup).unwrap();
        assert!(json.contains("embedding_b64"), "{json}");
        assert!(
            !json.contains("0.75") && !json.contains("embedding\":["),
            "向量不能以 JSON 数字数组导出：{json}"
        );

        let parsed: MemoryBackup = serde_json::from_str(&json).unwrap();
        let restored = parsed.memories.into_iter().next().unwrap().into_entry();
        assert_eq!(restored.embedding, entry().embedding);
        assert_eq!(restored.content, "用户喜欢猫");
        assert_eq!(restored.memory_type, MemoryType::Preference);
        assert_eq!(restored.access_count, 3);
    }

    /// 不含向量的导出必须标记，且条目的向量字段为空
    #[test]
    fn omitted_embeddings_are_flagged() {
        let backup = MemoryBackup {
            version: 2,
            exported_at: "now".to_string(),
            embeddings_omitted: true,
            memories: vec![MemoryBackupEntry::from_entry(&entry(), false)],
        };
        let json = serde_json::to_string(&backup).unwrap();
        let parsed: MemoryBackup = serde_json::from_str(&json).unwrap();
        assert!(parsed.embeddings_omitted);
        assert!(parsed.memories[0].embedding.is_none());
        assert!(parsed.memories[0].embedding_b64.is_none());
    }

    /// 旧版备份（embedding 为 JSON 数字数组）必须继续可导入
    #[test]
    fn legacy_backup_format_still_imports() {
        let legacy = r#"{
            "version": 1,
            "exported_at": "old",
            "memories": [{
                "id": "m-old",
                "content": "旧格式",
                "memory_type": "fact",
                "importance": 0.4,
                "embedding": [0.5, 0.5],
                "source_session": "s1",
                "created_at": "2026-01-01T00:00:00+00:00",
                "last_accessed": "2026-01-01T00:00:00+00:00",
                "access_count": 0
            }]
        }"#;
        let parsed: MemoryBackup = serde_json::from_str(legacy).unwrap();
        let imported = parsed.memories.into_iter().next().unwrap().into_entry();
        assert_eq!(imported.embedding, Some(vec![0.5, 0.5]));
        assert_eq!(imported.content, "旧格式");
    }
}
