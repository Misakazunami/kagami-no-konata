use anyhow::Result;
use chrono::Utc;
use uuid::Uuid;

use crate::agent::context::Message;
use crate::llm::proxy::LlmProxy;
use crate::llm::types::LlmMessage;
use crate::store::memory_store::{MemoryEntry, MemoryType};

/// 从对话中提取的原始记忆事实
#[derive(Debug, serde::Deserialize)]
struct ExtractedFact {
    content: String,
    #[serde(rename = "type")]
    fact_type: String,
    #[serde(default)]
    action: String, // "new" | "update" | "skip"
    #[serde(default)]
    update_id: String, // 要更新的记忆 ID（action=update 时使用）
}

/// 记忆提取器
pub struct MemoryExtractor;

impl MemoryExtractor {
    /// 从对话中提取记忆（参考已有记忆去重）
    pub async fn extract(
        llm_provider: &crate::config::types::LlmProvider,
        messages: &[Message],
        session_id: &str,
        existing_memories: &[MemoryEntry],
    ) -> Result<Vec<MemoryEntry>> {
        // 构造对话摘要（仅使用用户消息，避免 AI 回复中的虚构内容污染记忆）
        let conversation_text = messages
            .iter()
            .filter(|m| m.role == crate::agent::context::Role::User)
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");

        if conversation_text.trim().is_empty() {
            return Ok(Vec::new());
        }

        // 构造已有记忆上下文
        let existing_text = if !existing_memories.is_empty() {
            let mut lines = Vec::new();
            for mem in existing_memories.iter().take(20) {
                let type_label = match mem.memory_type {
                    MemoryType::Fact => "事实",
                    MemoryType::Preference => "偏好",
                    MemoryType::Experience => "经历",
                    MemoryType::Emotional => "情感",
                };
                lines.push(format!("[{}] {}（ID: {}）", type_label, mem.content, mem.id));
            }
            format!(
                "\n\n已有记忆（如果新信息与已有记忆重复或矛盾，请使用 update 操作更新，skip 表示跳过）：\n{}",
                lines.join("\n")
            )
        } else {
            String::new()
        };

        // 构造提取 prompt
        let prompt = format!(
            r#"请从以下用户的发言中提取关于用户本人的关键信息。包括：
- 事实（用户的背景、经历、职业、习惯等）
- 偏好（用户喜欢/不喜欢什么、风格偏好等）
- 经历（用户提到的计划、事件、故事等）
- 情感（用户的情绪状态、感受等）

注意：以下内容均为用户本人的发言，不是对话记录。只提取用户明确表达的个人信息，不要推测或编造。
{existing_text}

用户发言：
{conversation_text}

请以 JSON 数组格式返回，每条包含 content、type、action 字段。
- type: "fact" | "preference" | "experience" | "emotional"
- action: "new"（全新信息）| "update"（更新已有记忆，需提供 update_id）| "skip"（与已有记忆完全重复）
- update_id: 仅 action=update 时需要，填已有记忆的 ID

如果没有值得记忆的信息，返回空数组 []。
只返回 JSON，不要有其他文字。

示例：
[{{{{"content": "用户是一名软件工程师", "type": "fact", "action": "new"}}}}, {{{{"content": "用户喜欢猫", "type": "preference", "action": "update", "update_id": "abc-123"}}}}]"#
        );

        // 调用 LLM 提取
        let proxy = LlmProxy::new(llm_provider);
        let response = proxy
            .chat(vec![LlmMessage::user(prompt)])
            .await?;

        // 解析 JSON 响应
        let facts = parse_extracted_facts(&response);

        if facts.is_empty() {
            return Ok(Vec::new());
        }

        // 处理提取结果（跳过重复）
        let actionable: Vec<ExtractedFact> =
            facts.into_iter().filter(|f| f.action != "skip").collect();

        if actionable.is_empty() {
            return Ok(Vec::new());
        }

        // 内容未变化的 update 操作直接复用已有向量，其余统一批量嵌入（单次 API 调用）
        let mut planned: Vec<(ExtractedFact, Option<Vec<f32>>)> = Vec::with_capacity(actionable.len());
        let mut need_embed: Vec<String> = Vec::new();
        for fact in actionable {
            let reuse = if fact.action == "update" && !fact.update_id.is_empty() {
                existing_memories
                    .iter()
                    .find(|m| m.id == fact.update_id)
                    .filter(|m| m.content == fact.content)
                    .and_then(|m| m.embedding.clone())
            } else {
                None
            };
            if reuse.is_none() {
                need_embed.push(fact.content.clone());
            }
            planned.push((fact, reuse));
        }

        let embeddings = if need_embed.is_empty() {
            Vec::new()
        } else {
            match proxy.embed(need_embed).await {
                Ok(v) => v,
                Err(e) => {
                    // 绝不把"没有向量"的记忆写进库：它们不会被检索到（数据黑洞），
                    // 而且下一轮会重复提取一遍、白白多花一次 LLM 调用。
                    // 整轮放弃并如实记录，下次对话自然重试。
                    anyhow::bail!("记忆向量化失败，本轮不写入任何记忆（下次对话会重试）：{}", e);
                }
            }
        };
        let mut embeddings_iter = embeddings.into_iter();

        let mut entries = Vec::new();
        for (fact, reused_embedding) in planned {
            // 顺序对应：非复用条目依次消费批量嵌入结果
            let embedding = match reused_embedding {
                Some(e) => Some(e),
                None => embeddings_iter.next(),
            };

            let importance = calculate_importance(&fact.content, &fact.fact_type);

            let entry = MemoryEntry {
                id: if fact.action == "update" && !fact.update_id.is_empty() {
                    fact.update_id
                } else {
                    Uuid::new_v4().to_string()
                },
                content: fact.content,
                memory_type: MemoryType::from_str(&fact.fact_type),
                importance,
                embedding,
                source_session: session_id.to_string(),
                created_at: Utc::now().to_rfc3339(),
                last_accessed: Utc::now().to_rfc3339(),
                access_count: 0,
            };

            entries.push(entry);
        }

        Ok(entries)
    }
}

/// 解析 LLM 返回的提取结果
fn parse_extracted_facts(response: &str) -> Vec<ExtractedFact> {
    if let Ok(facts) = serde_json::from_str::<Vec<ExtractedFact>>(response) {
        return facts;
    }

    let trimmed = response.trim();
    if let Some(start) = trimmed.find('[') {
        if let Some(end) = trimmed.rfind(']') {
            let json_str = &trimmed[start..=end];
            if let Ok(facts) = serde_json::from_str::<Vec<ExtractedFact>>(json_str) {
                return facts;
            }
        }
    }

    Vec::new()
}

/// 计算记忆重要性
fn calculate_importance(content: &str, fact_type: &str) -> f32 {
    let base = match fact_type {
        "fact" => 0.6,
        "preference" => 0.7,
        "experience" => 0.5,
        "emotional" => 0.6,
        _ => 0.5,
    };
    let length_bonus = (content.chars().count() as f32 / 100.0).min(0.2);
    (base + length_bonus).min(1.0)
}
