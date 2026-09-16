use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 消息角色
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::System => write!(f, "system"),
            Role::User => write!(f, "user"),
            Role::Assistant => write!(f, "assistant"),
        }
    }
}

/// 对话消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub content: String,
    pub timestamp: String,
    pub session_id: String,
    #[serde(default)]
    pub token_count: i64,
    #[serde(default)]
    pub thinking_ms: i64,
    /// 思考内容（模型的推理过程，仅 assistant 消息可能包含）
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// 生成这条回复的模型标签（仅 assistant 消息，自动选择下主/子模型不同，
    /// 落库后历史消息也能显示"这条是谁答的"）
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            role,
            content: content.into(),
            timestamp: Utc::now().to_rfc3339(),
            session_id: session_id.into(),
            token_count: 0,
            thinking_ms: 0,
            thinking: None,
            model: None,
        }
    }
}

/// 会话信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub persona_id: String,
    #[serde(default = "default_session_type")]
    pub session_type: String,
    #[serde(default = "default_task_mode")]
    pub task_mode: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// 会话级模型选择（手动 / 自动 + 深度思考开关）
    ///
    /// `None` = 跟随全局活跃提供商（与未引入该功能时一致）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_pref: Option<crate::llm::router::SessionModelPref>,
    pub created_at: String,
    pub updated_at: String,
}

fn default_session_type() -> String {
    "chat".to_string()
}

fn default_task_mode() -> String {
    "plan".to_string()
}
