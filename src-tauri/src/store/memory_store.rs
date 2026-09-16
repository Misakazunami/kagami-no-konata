use anyhow::Result;
use chrono::Utc;
use rusqlite::Connection;

/// 记忆类型
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
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

/// 读取记忆行中的嵌入向量：优先 BLOB，降级解析旧格式 JSON
fn row_embedding(row_embedding_json: Option<String>, row_blob: Option<Vec<u8>>) -> Option<Vec<f32>> {
    if let Some(blob) = row_blob {
        return blob_to_vector(&blob);
    }
    row_embedding_json.and_then(|j| serde_json::from_str(&j).ok())
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
    pub fn recall(&self, query_embedding: &[f32], top_k: usize) -> Result<Vec<MemoryEntry>> {
        let query_norm = normalize_vector(query_embedding);

        let mut stmt = self.conn.prepare(
            "SELECT id, content, memory_type, importance, embedding, embedding_blob, source_session, created_at, last_accessed, access_count FROM memories WHERE embedding IS NOT NULL OR embedding_blob IS NOT NULL",
        )?;

        let mut scored: Vec<(f32, MemoryEntry)> = Vec::new();
        let mut dim_mismatch = 0usize;

        let rows = stmt.query_map([], |row| {
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

            if let Some(emb) = embedding {
                // 维度不一致（换过 embedding 模型 / 导入他人备份）：
                // 直接跳过，避免用截断后的点积给出错误排序
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

                // 综合得分 = 相似度 * 0.7 + 重要性 * 0.3
                let score = similarity * 0.7 + importance * 0.3;

                scored.push((
                    score,
                    MemoryEntry {
                        id,
                        content,
                        memory_type: MemoryType::from_str(&mem_type),
                        importance,
                        embedding: Some(emb),
                        source_session: source,
                        created_at: created,
                        last_accessed: accessed,
                        access_count: count,
                    },
                ));
            }
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

        entry.content = entry
            .content
            .trim()
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
