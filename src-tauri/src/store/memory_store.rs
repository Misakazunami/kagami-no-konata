use anyhow::Result;
use chrono::Utc;
use rusqlite::Connection;

/// 记忆类型
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    #[default]
    Fact,
    Preference,
    Experience,
    Emotional,
}

impl std::fmt::Display for MemoryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryType::Fact => write!(f, "fact"),
            MemoryType::Preference => write!(f, "preference"),
            MemoryType::Experience => write!(f, "experience"),
            MemoryType::Emotional => write!(f, "emotional"),
        }
    }
}

impl MemoryType {
    pub fn from_str(s: &str) -> Self {
        match s {
            "fact" => MemoryType::Fact,
            "preference" => MemoryType::Preference,
            "experience" => MemoryType::Experience,
            "emotional" => MemoryType::Emotional,
            _ => MemoryType::Fact,
        }
    }
}

/// 记忆条目
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub memory_type: MemoryType,
    pub importance: f32,
    pub embedding: Option<Vec<f32>>,
    pub source_session: String,
    pub created_at: String,
    pub last_accessed: String,
    pub access_count: i32,
}

/// 记忆存储
pub struct MemoryStore {
    conn: Connection,
}

/// 归一化向量（L2），零向量原样返回
fn normalize_vector(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        v.to_vec()
    } else {
        v.iter().map(|x| x / norm).collect()
    }
}

/// 向量序列化为 f32 小端 BLOB
fn vector_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// BLOB 反序列化为向量
fn blob_to_vector(blob: &[u8]) -> Option<Vec<f32>> {
    if blob.is_empty() || !blob.len().is_multiple_of(4) {
        return None;
    }
    Some(
        blob.chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    )
}

/// 向量 → Base64（备份导出用）
///
/// 直接以 JSON 数字数组导出时，一个 1536 维向量约 15–20 KB；同样内容用
/// f32 小端 + Base64 只要约 8 KB，把备份体积压掉一半以上，且能原样导回。
pub fn encode_embedding_b64(v: &[f32]) -> String {
    encode_b64(&vector_to_blob(v))
}

/// Base64 → 向量（长度/合法性校验由调用方与 `sanitize_imported` 负责）
pub fn decode_embedding_b64(text: &str) -> Option<Vec<f32>> {
    blob_to_vector(&decode_b64(text)?)
}

const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode_b64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64_ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn decode_b64(text: &str) -> Option<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        } as u32;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// 已归一化向量的点积（即余弦相似度）
///
/// 维度不一致时返回 `None`（而不是像 `zip` 那样静默截断 ——
/// 换过 embedding 模型或导入他人备份后，截断会算出无意义的相似度）
fn dot_product(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() {
        return None;
    }
    Some(a.iter().zip(b.iter()).map(|(x, y)| x * y).sum())
}

/// 单条记忆的内容长度上限（防止超长文本被注入 system prompt / 撑爆数据库）
pub const MAX_MEMORY_CONTENT_CHARS: usize = 2000;
/// 单条记忆允许的最大向量维度（防止导入超大向量撑爆内存）
pub const MAX_EMBEDDING_DIM: usize = 8192;
/// 一次性导入的记忆条数上限
pub const MAX_IMPORT_ENTRIES: usize = 50_000;
/// 单次召回最多参与打分的记忆条数
///
/// 向量要在 Rust 侧反序列化后逐条打分：导入上限允许 5 万条 × 8192 维，
/// 全部加载会吃掉数百 MB 内存。这里按重要性取候选集，超出部分不参与本轮召回
/// （存储与界面展示不受影响）。
pub const RECALL_CANDIDATE_LIMIT: usize = 5_000;

/// 读取记忆行中的嵌入向量：优先 BLOB，降级解析旧格式 JSON
fn row_embedding(row_embedding_json: Option<String>, row_blob: Option<Vec<u8>>) -> Option<Vec<f32>> {
    if let Some(blob) = row_blob {
        return blob_to_vector(&blob);
    }
    row_embedding_json.and_then(|j| serde_json::from_str(&j).ok())
}

/// 去掉控制字符（保留换行/制表等正常排版字符）
///
/// 注入 system prompt 的文本里混入 `\x00`、`\x1b`（ANSI 转义）或各类
/// 零宽控制符既会把提示词搅乱，也是提示注入常用的混淆手段。
fn strip_control_chars(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
        .collect()
}

impl MemoryStore {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    /// 存储记忆（upsert：ID 存在则更新）
    ///
    /// 嵌入向量在写入前归一化并以 BLOB 形式存储，
    /// 检索时无需再计算范数（余弦退化为点积）。
    pub fn store_memory(&self, memory: &MemoryEntry) -> Result<()> {
        Self::upsert(&self.conn, memory)?;
        Ok(())
    }

    /// 单条写入的实际实现（同时供事务化批量导入复用）
    fn upsert(conn: &Connection, memory: &MemoryEntry) -> rusqlite::Result<()> {
        let embedding_blob = memory
            .embedding
            .as_ref()
            .map(|e| vector_to_blob(&normalize_vector(e)));

        conn.execute(
            "INSERT INTO memories (id, content, memory_type, importance, embedding, embedding_blob, source_session, created_at, last_accessed, access_count)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
               content = excluded.content,
               memory_type = excluded.memory_type,
               importance = excluded.importance,
               embedding_blob = COALESCE(excluded.embedding_blob, memories.embedding_blob),
               last_accessed = excluded.last_accessed",
            rusqlite::params![
                memory.id,
                memory.content,
                memory.memory_type.to_string(),
                memory.importance,
                embedding_blob,
                memory.source_session,
                memory.created_at,
                memory.last_accessed,
                memory.access_count,
            ],
        )?;
        Ok(())
    }

    /// 事务化批量导入
    ///
    /// 返回 `(成功数, 失败数)`。整批清空 + 写入在同一个事务里完成，
    /// 任一步出错都会整体回滚，不会出现"清空成功但写入失败"的静默数据丢失。
    pub fn import_memories(&self, entries: &[MemoryEntry], merge: bool) -> Result<(usize, usize)> {
        let tx = self.conn.unchecked_transaction()?;

        if !merge {
            tx.execute("DELETE FROM memories", [])?;
        }

        let mut imported = 0usize;
        let mut failed = 0usize;
        for entry in entries {
            match Self::upsert(&tx, entry) {
                Ok(()) => imported += 1,
                Err(e) => {
                    failed += 1;
                    eprintln!("[memory] 导入记忆失败 {}: {}", entry.id, e);
                }
            }
        }

        tx.commit()?;
        Ok((imported, failed))
    }

    /// 向量检索：返回综合得分 top_k
    ///
    /// 得分 = 余弦相似度 * 0.7 + 重要性 * 0.3；
    /// 向量均已归一化存储，余弦相似度直接用点积计算。
    /// 没有向量的记忆（导入数据 / 嵌入失败）以相似度 0（纯重要性）参与排序，
    /// 保证它们不会成为"界面上存在、模型永远看不到"的数据黑洞。
    pub fn recall(&self, query_embedding: &[f32], top_k: usize) -> Result<Vec<MemoryEntry>> {
        let query_norm = normalize_vector(query_embedding);

        let mut stmt = self.conn.prepare(
            "SELECT id, content, memory_type, importance, embedding, embedding_blob, source_session, created_at, last_accessed, access_count
             FROM memories
             ORDER BY importance DESC, created_at DESC
             LIMIT ?1",
        )?;

        let mut scored: Vec<(f32, MemoryEntry)> = Vec::new();
        let mut dim_mismatch = 0usize;

        let rows = stmt.query_map([RECALL_CANDIDATE_LIMIT as i64], |row| {
            let embedding_json: Option<String> = row.get(4)?;
            let blob: Option<Vec<u8>> = row.get(5)?;

            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, f32>(3)?,
                row_embedding(embedding_json, blob),
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, i32>(9)?,
            ))
        })?;

        for row in rows {
            let (id, content, mem_type, importance, embedding, source, created, accessed, count) =
                row?;

            // 综合得分 = 相似度 * 0.7 + 重要性 * 0.3
            let (similarity, embedding) = match embedding {
                Some(emb) => {
                    // 维度不一致（换过 embedding 模型 / 导入他人备份）：
                    // 跳过这一条，避免用截断后的点积给出错误排序
                    if emb.len() != query_norm.len() {
                        dim_mismatch += 1;
                        continue;
                    }
                    // 已归一化：点积即余弦相似度；兼容未回填的旧行仍走完整余弦公式
                    let Some(similarity) = (if is_normalized(&emb) {
                        dot_product(&query_norm, &emb)
                    } else {
                        Some(cosine_similarity(&query_norm, &emb))
                    }) else {
                        continue;
                    };
                    (similarity, Some(emb))
                }
                // 没有向量的记忆（导入的旧备份 / 嵌入服务当时不可用）：
                // 按重要性兜底参与排序，而不是永远不可检索——否则这些记忆
                // 会在界面上存在、却永远不会被模型看到（真实的数据黑洞）。
                None => (0.0, None),
            };
            let score = similarity * 0.7 + importance * 0.3;

            scored.push((
                score,
                MemoryEntry {
                    id,
                    content,
                    memory_type: MemoryType::from_str(&mem_type),
                    importance,
                    embedding,
                    source_session: source,
                    created_at: created,
                    last_accessed: accessed,
                    access_count: count,
                },
            ));
        }

        if dim_mismatch > 0 {
            eprintln!(
                "[memory] {} 条记忆的向量维度与当前 embedding 模型不一致（期望 {}），已跳过",
                dim_mismatch,
                query_norm.len()
            );
        }

        // 按得分降序排序
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        // 更新访问记录并返回 top_k
        let now = Utc::now().to_rfc3339();
        let results: Vec<MemoryEntry> = scored
            .into_iter()
            .take(top_k)
            .map(|(_, mut entry)| {
                entry.access_count += 1;
                entry.last_accessed = now.clone();
                let _ = self.conn.execute(
                    "UPDATE memories SET access_count = access_count + 1, last_accessed = ?1 WHERE id = ?2",
                    rusqlite::params![now, entry.id],
                );
                entry
            })
            .collect();

        Ok(results)
    }

    /// 获取所有记忆（含向量，供备份导出使用）
    pub fn list_memories(&self, limit: Option<usize>) -> Result<Vec<MemoryEntry>> {
        let sql = match limit {
            Some(n) => format!(
                "SELECT id, content, memory_type, importance, embedding, embedding_blob, source_session, created_at, last_accessed, access_count FROM memories ORDER BY created_at DESC LIMIT {}",
                n
            ),
            None => "SELECT id, content, memory_type, importance, embedding, embedding_blob, source_session, created_at, last_accessed, access_count FROM memories ORDER BY created_at DESC".to_string(),
        };

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            let embedding_json: Option<String> = row.get(4)?;
            let blob: Option<Vec<u8>> = row.get(5)?;
            let embedding = row_embedding(embedding_json, blob);

            Ok(MemoryEntry {
                id: row.get(0)?,
                content: row.get(1)?,
                memory_type: MemoryType::from_str(&row.get::<_, String>(2)?),
                importance: row.get(3)?,
                embedding,
                source_session: row.get(6)?,
                created_at: row.get(7)?,
                last_accessed: row.get(8)?,
                access_count: row.get(9)?,
            })
        })?;

        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 记忆条数（只查 COUNT：`get_app_status` 只需要数字，不该把全部
    /// 1536 维向量反序列化一遍）
    pub fn count_memories(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
        Ok(count.max(0) as usize)
    }

    /// 供界面展示的记忆列表：不读取/不返回向量
    ///
    /// 界面只用 content / type / importance，而单个向量是 1536 维浮点数组，
    /// 每次打开设置页把 50 条向量（数十万个 JSON 数字）跨 IPC 传到前端纯属浪费。
    pub fn list_memories_for_ui(&self, limit: Option<usize>) -> Result<Vec<MemoryEntry>> {
        let sql = match limit {
            Some(n) => format!(
                "SELECT id, content, memory_type, importance, source_session, created_at, last_accessed, access_count FROM memories ORDER BY created_at DESC LIMIT {}",
                n
            ),
            None => "SELECT id, content, memory_type, importance, source_session, created_at, last_accessed, access_count FROM memories ORDER BY created_at DESC".to_string(),
        };

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            Ok(MemoryEntry {
                id: row.get(0)?,
                content: row.get(1)?,
                memory_type: MemoryType::from_str(&row.get::<_, String>(2)?),
                importance: row.get(3)?,
                embedding: None,
                source_session: row.get(4)?,
                created_at: row.get(5)?,
                last_accessed: row.get(6)?,
                access_count: row.get(7)?,
            })
        })?;

        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

/// 清洗外部导入的记忆条目（返回 None 表示该条应被跳过）
///
/// 备份 JSON 来自外部，`content` / `importance` / `embedding` 均不可信：
/// - `content` 超长会污染之后每一轮对话的 system prompt
/// - `importance` 越界（例如 1e30）会让该条记忆在检索中永久霸榜
/// - `embedding` 维度异常会撑爆内存、维度不符则让检索结果变成噪声
pub fn sanitize_imported(mut entry: MemoryEntry) -> Option<MemoryEntry> {
    entry.id = entry.id.trim().chars().take(128).collect();
    if entry.id.is_empty() {
        return None;
    }

    entry.content = strip_control_chars(entry.content.trim());
    entry.content = entry
        .content
        .chars()
        .take(MAX_MEMORY_CONTENT_CHARS)
        .collect();
    if entry.content.is_empty() {
        return None;
    }

        if !entry.importance.is_finite() {
            entry.importance = 0.5;
        }
        entry.importance = entry.importance.clamp(0.0, 1.0);

        if let Some(embedding) = &entry.embedding {
            let invalid = embedding.is_empty()
                || embedding.len() > MAX_EMBEDDING_DIM
                || embedding.iter().any(|v| !v.is_finite());
            if invalid {
                entry.embedding = None;
            }
        }

        entry.source_session = entry.source_session.chars().take(128).collect();
        Some(entry)
    }

    /// 启动时一次性回填：将旧格式 JSON 向量转换为归一化 BLOB
    pub fn backfill_embeddings(&self) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT id, embedding FROM memories WHERE embedding IS NOT NULL AND embedding_blob IS NULL",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        if rows.is_empty() {
            return Ok(());
        }

        let tx = self.conn.unchecked_transaction()?;
        for (id, json) in rows {
            if let Ok(embedding) = serde_json::from_str::<Vec<f32>>(&json) {
                let blob = vector_to_blob(&normalize_vector(&embedding));
                tx.execute(
                    "UPDATE memories SET embedding_blob = ?1 WHERE id = ?2",
                    rusqlite::params![blob, id],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 删除记忆
    pub fn delete_memory(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![id])?;
        Ok(())
    }

    /// 清除某会话的来源标记（会话被删除时调用）
    ///
    /// `memories` 没有外键约束（记忆要跨会话长期存在），不清理的话会留下
    /// 指向已删除会话的悬空 `source_session`。记忆本身保留，只断开来源
    /// （写成空串而不是 NULL：读取路径按非空 `String` 反序列化）。
    pub fn clear_source_session(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE memories SET source_session = '' WHERE source_session = ?1",
            rusqlite::params![session_id],
        )?;
        Ok(())
    }

    /// 清空所有记忆
    pub fn clear_memories(&self) -> Result<()> {
        self.conn.execute("DELETE FROM memories", [])?;
        Ok(())
    }
}

/// 判断向量是否已归一化（范数接近 1）
fn is_normalized(v: &[f32]) -> bool {
    let norm_sq: f32 = v.iter().map(|x| x * x).sum();
    (norm_sq - 1.0).abs() < 0.01
}

/// 余弦相似度
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum::<f32>();
    let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(content: &str) -> MemoryEntry {
        MemoryEntry {
            id: "m1".to_string(),
            content: content.to_string(),
            memory_type: MemoryType::Fact,
            importance: 0.5,
            embedding: None,
            source_session: "s1".to_string(),
            created_at: String::new(),
            last_accessed: String::new(),
            access_count: 0,
        }
    }

    /// 导入的记忆会注入每一轮 system prompt：控制字符（含 ANSI 转义与
    /// 零宽混淆字符）必须被清掉，正常换行/制表保留
    #[test]
    fn sanitize_imported_strips_control_chars() {
        let clean = MemoryStore::sanitize_imported(entry("多行\n内容\t缩进\x07\x1b[31m红\x00")).unwrap();
        assert!(!clean.content.contains('\x07'), "{:?}", clean.content);
        assert!(!clean.content.contains('\x1b'), "{:?}", clean.content);
        assert!(!clean.content.contains('\x00'), "{:?}", clean.content);
        assert!(clean.content.contains('\n') && clean.content.contains('\t'));

        // 纯控制字符 → 清洗后为空，整条跳过
        assert!(MemoryStore::sanitize_imported(entry("\x00\x07")).is_none());
    }

    #[test]
    fn sanitize_imported_clamps_bad_values() {
        let mut bad = entry("有效内容");
        bad.importance = f32::INFINITY;
        let clean = MemoryStore::sanitize_imported(bad).unwrap();
        assert_eq!(clean.importance, 0.5);

        let mut huge = entry("x");
        huge.embedding = Some(vec![0.0; MAX_EMBEDDING_DIM + 1]);
        assert!(MemoryStore::sanitize_imported(huge).unwrap().embedding.is_none());
    }

    /// 备份里的向量用 Base64 紧凑编码，必须无损往返
    #[test]
    fn embedding_b64_round_trips() {
        let values: Vec<f32> = vec![0.0, 1.5, -2.25, 1e-6, 123.456, f32::MIN];
        let encoded = encode_embedding_b64(&values);
        assert!(!encoded.contains(','), "不能是 JSON 数字数组：{encoded}");
        let decoded = decode_embedding_b64(&encoded).expect("必须能解回");
        assert_eq!(values, decoded);

        // 非法字符与空串要给出 None，而不是 panic/静默截断
        assert!(decode_embedding_b64("!!!").is_none());
        assert!(decode_embedding_b64("").is_none(), "空向量不是合法嵌入");
        // 6 个 Base64 字符 = 4 字节 = 1 个 f32；不足 4 字节的载荷必须拒绝
        assert_eq!(decode_embedding_b64("AAAAAA"), Some(vec![0.0f32]));
        assert!(decode_embedding_b64("AAAA").is_none(), "不足一个 f32 的载荷应拒绝");
    }

    /// 召回必须有候选集上限，避免超大备份（5 万条 × 高维向量）把内存打爆
    #[test]
    fn recall_caps_candidate_set() {
        let dir = std::env::temp_dir().join(format!(
            "konata-memory-cap-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = crate::store::db::init_db(&dir).unwrap();
        let store = MemoryStore::new(conn);

        let total = RECALL_CANDIDATE_LIMIT + 10;
        let entries: Vec<MemoryEntry> = (0..total)
            .map(|index| {
                let mut memory = entry(&format!("记忆 {index}"));
                memory.id = format!("m{index}");
                memory.importance = 0.5;
                memory.embedding = Some(vec![1.0, 0.0]);
                memory
            })
            .collect();
        let (imported, failed) = store.import_memories(&entries, true).unwrap();
        assert_eq!((imported, failed), (total, 0));

        let hits = store.recall(&[1.0, 0.0], total).unwrap();
        assert_eq!(
            hits.len(),
            RECALL_CANDIDATE_LIMIT,
            "参与打分的候选集必须有上限"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 会话删除后来源标记要断开：记忆保留、读取路径不能因 NULL 而失败
    #[test]
    fn clear_source_session_keeps_memory_readable() {
        let dir = std::env::temp_dir().join(format!(
            "konata-memory-src-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = crate::store::db::init_db(&dir).unwrap();
        let store = MemoryStore::new(conn);

        let mut memory = entry("来自旧会话");
        memory.source_session = "gone-session".to_string();
        memory.embedding = Some(vec![1.0, 0.0]);
        store.store_memory(&memory).unwrap();

        store.clear_source_session("gone-session").unwrap();

        let listed = store.list_memories(None).unwrap();
        assert_eq!(listed.len(), 1, "记忆本身必须保留");
        assert_eq!(listed[0].source_session, "");
        assert!(store.recall(&[1.0, 0.0], 5).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没有向量的记忆必须仍可被检索到（否则导入/嵌入失败的数据永远不可见）
    #[test]
    fn memories_without_embeddings_still_participate_in_recall() {
        let dir = std::env::temp_dir().join(format!(
            "konata-memory-recall-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = crate::store::db::init_db(&dir).unwrap();
        let store = MemoryStore::new(conn);

        let mut with_vec = entry("有向量的记忆");
        with_vec.importance = 0.5;
        with_vec.embedding = Some(vec![1.0, 0.0]);
        store.store_memory(&with_vec).unwrap();

        let mut no_vec = entry("没有向量的记忆");
        no_vec.id = "m2".to_string();
        no_vec.content = "没有向量的记忆".to_string();
        no_vec.importance = 0.9;
        no_vec.embedding = None;
        store.store_memory(&no_vec).unwrap();

        let hits = store.recall(&[1.0, 0.0], 10).unwrap();
        assert_eq!(hits.len(), 2, "两条都应参与召回：{hits:?}");
        assert!(hits.iter().any(|m| m.content == "没有向量的记忆"));
        // 相似度高的仍然排在前面（兜底不能反超正常相似度）
        assert_eq!(hits[0].content, "有向量的记忆");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
