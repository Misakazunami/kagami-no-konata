//! 会话级任务计划（模型可写、用户可见）
//!
//! 为什么需要它：工具结果**不跨轮保留**，而一轮生成又有步数上限（默认 8 轮）。
//! 长任务（"先看 5 个文件、改 3 处、再跑测试"）如果计划只活在模型脑子里，
//! 用户中途插一句话、或者步数耗尽，进度就丢了。
//!
//! 因此把计划做成**显式的会话级状态**：
//! - 模型通过 `update_plan` 工具写（`Permission::WriteApp`，不需要审批）；
//! - 每次生成都会把计划注入 system prompt（`prompt_section`），模型因此知道
//!   自己做到哪了，用户也在界面上看到同一份进度；
//! - 它是**模型自己写的结构化数据**（标题短短一行），不是外部内容，
//!   所以不存在 `tool_result` 那种提示注入面——这正是不做"跨轮工具结果记忆"
//!   也能拿到大部分连续性的原因。

use serde::{Deserialize, Serialize};

use crate::store::chat_store::MAX_PLAN_ITEMS;

/// 单个计划项的状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    Doing,
    Done,
    Blocked,
}

impl PlanStatus {
    /// 从模型给的字符串解析；无法识别一律按 `Pending`（不能因为写错一个词就整条失败）
    pub fn parse(raw: &str) -> PlanStatus {
        match raw.trim().to_ascii_lowercase().as_str() {
            "doing" | "in_progress" | "running" => PlanStatus::Doing,
            "done" | "completed" | "complete" => PlanStatus::Done,
            "blocked" | "failed" | "stuck" => PlanStatus::Blocked,
            _ => PlanStatus::Pending,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PlanStatus::Pending => "pending",
            PlanStatus::Doing => "doing",
            PlanStatus::Done => "done",
            PlanStatus::Blocked => "blocked",
        }
    }

    /// 提示词里的勾选框样式（纯文本，不依赖任何 markdown 渲染）
    pub fn checkbox(self) -> &'static str {
        match self {
            PlanStatus::Pending => "[ ]",
            PlanStatus::Doing => "[>]",
            PlanStatus::Done => "[x]",
            PlanStatus::Blocked => "[!]",
        }
    }

    /// 界面上的中文标签
    pub fn label(self) -> &'static str {
        match self {
            PlanStatus::Pending => "待办",
            PlanStatus::Doing => "进行中",
            PlanStatus::Done => "已完成",
            PlanStatus::Blocked => "受阻",
        }
    }
}

/// 计划项
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    pub title: String,
    #[serde(default = "default_status")]
    pub status: PlanStatus,
}

fn default_status() -> PlanStatus {
    PlanStatus::Pending
}

/// 一个会话的完整计划
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPlan {
    pub items: Vec<PlanItem>,
    /// 可选的补充说明（一句话，例如"等用户确认改动范围"）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// 最后一次更新时间（RFC3339）
    #[serde(default)]
    pub updated_at: String,
}

impl SessionPlan {
    pub fn new(items: Vec<PlanItem>, note: Option<String>) -> Self {
        Self {
            items,
            note: note.filter(|n| !n.trim().is_empty()),
            updated_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 从库里读出来时解析；坏数据按"没有计划"处理（不能因为一行 JSON 让整轮对话失败）
    pub fn from_json(raw: &str) -> Option<Self> {
        serde_json::from_str::<Self>(raw).ok()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{\"items\":[]}".to_string())
    }

    /// 各状态的计数：`(完成, 进行中, 待办, 受阻)`
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut done = 0;
        let mut doing = 0;
        let mut pending = 0;
        let mut blocked = 0;
        for item in &self.items {
            match item.status {
                PlanStatus::Done => done += 1,
                PlanStatus::Doing => doing += 1,
                PlanStatus::Pending => pending += 1,
                PlanStatus::Blocked => blocked += 1,
            }
        }
        (done, doing, pending, blocked)
    }

    /// 给用户/模型看的一行摘要
    pub fn summary(&self) -> String {
        if self.items.is_empty() {
            return "计划已清空".to_string();
        }
        let (done, doing, pending, blocked) = self.counts();
        let mut parts = vec![format!("共 {} 项", self.items.len())];
        parts.push(format!("完成 {}", done));
        if doing > 0 {
            parts.push(format!("进行中 {}", doing));
        }
        if pending > 0 {
            parts.push(format!("待办 {}", pending));
        }
        if blocked > 0 {
            parts.push(format!("受阻 {}", blocked));
        }
        parts.join(" · ")
    }

    /// 注入 system prompt 的段落
    ///
    /// 放在 system 段而不是对话历史里：它不进摘要、不进记忆提取，
    /// 也不会被当成用户的发言。
    pub fn prompt_section(&self) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        let mut out = String::from("\n\n【当前任务计划】\n");
        out.push_str("（这是你自己维护的进度，用户也看得到。完成一步就更新它，不要在没有进展时反复改写）\n");
        for (index, item) in self.items.iter().enumerate() {
            out.push_str(&format!(
                "{}. {} {}\n",
                index + 1,
                item.status.checkbox(),
                item.title
            ));
        }
        if let Some(note) = self.note.as_deref() {
            out.push_str(&format!("备注：{}\n", note));
        }
        Some(out)
    }
}

/// 校验并规整模型给的计划项
///
/// 返回 `Err` 时由 runner 回灌给模型自我修正；条数上限与标题长度都取"够用且不被滥用"的值。
pub fn sanitize_items(raw: &serde_json::Value) -> Result<Vec<PlanItem>, String> {
    let Some(array) = raw.as_array() else {
        return Err("items 必须是数组".to_string());
    };
    if array.len() > MAX_PLAN_ITEMS {
        return Err(format!("计划项过多（上限 {} 项）", MAX_PLAN_ITEMS));
    }
    let mut items = Vec::with_capacity(array.len());
    for (index, entry) in array.iter().enumerate() {
        // 允许两种写法：字符串（只有标题，按待办处理）或对象
        let (title, status) = match entry {
            serde_json::Value::String(title) => (title.clone(), PlanStatus::Pending),
            serde_json::Value::Object(map) => {
                let title = map
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let status = map
                    .get("status")
                    .and_then(|v| v.as_str())
                    .map(PlanStatus::parse)
                    .unwrap_or(PlanStatus::Pending);
                (title, status)
            }
            _ => return Err(format!("items[{}] 必须是对象或字符串", index)),
        };
        if title.is_empty() {
            return Err(format!("items[{}] 缺少 title", index));
        }
        if title.chars().count() > 200 {
            return Err(format!("items[{}] 标题过长（上限 200 字）", index));
        }
        items.push(PlanItem { title, status });
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan() -> SessionPlan {
        SessionPlan::new(
            vec![
                PlanItem {
                    title: "读代码".to_string(),
                    status: PlanStatus::Done,
                },
                PlanItem {
                    title: "改实现".to_string(),
                    status: PlanStatus::Doing,
                },
                PlanItem {
                    title: "跑测试".to_string(),
                    status: PlanStatus::Pending,
                },
            ],
            Some("等用户确认范围".to_string()),
        )
    }

    #[test]
    fn status_parsing_is_forgiving_but_fail_safe() {
        assert_eq!(PlanStatus::parse("DONE"), PlanStatus::Done);
        assert_eq!(PlanStatus::parse("in_progress"), PlanStatus::Doing);
        assert_eq!(PlanStatus::parse("blocked"), PlanStatus::Blocked);
        // 认不出来按待办处理，而不是整条计划失败
        assert_eq!(PlanStatus::parse("wat"), PlanStatus::Pending);
        assert_eq!(PlanStatus::parse(""), PlanStatus::Pending);
    }

    #[test]
    fn json_round_trip_survives_bad_data() {
        let original = plan();
        let restored = SessionPlan::from_json(&original.to_json()).expect("往返成功");
        assert_eq!(restored.items, original.items);
        assert_eq!(restored.note, original.note);
        // 坏数据不能 panic，只当作"没有计划"
        assert!(SessionPlan::from_json("{ 不是 json").is_none());
        assert!(SessionPlan::from_json("null").is_none());
    }

    #[test]
    fn counts_and_summary_reflect_statuses() {
        let plan = plan();
        assert_eq!(plan.counts(), (1, 1, 1, 0));
        let summary = plan.summary();
        assert!(summary.contains("共 3 项"), "{summary}");
        assert!(summary.contains("完成 1"), "{summary}");
        assert!(summary.contains("进行中 1"), "{summary}");
        // 没有受阻项时不出现"受阻"
        assert!(!summary.contains("受阻"), "{summary}");
    }

    #[test]
    fn prompt_section_carries_progress_and_note() {
        let section = plan().prompt_section().expect("有计划就有段落");
        assert!(section.contains("【当前任务计划】"), "{section}");
        assert!(section.contains("[x] 读代码"), "{section}");
        assert!(section.contains("[>] 改实现"), "{section}");
        assert!(section.contains("[ ] 跑测试"), "{section}");
        assert!(section.contains("备注：等用户确认范围"), "{section}");
        // 空计划不注入任何东西（避免污染纯聊天链路的提示词）
        assert!(SessionPlan::new(Vec::new(), None).prompt_section().is_none());
    }

    #[test]
    fn sanitize_accepts_plain_strings_and_objects() {
        let items = sanitize_items(&json!(["读代码", {"title": "改实现", "status": "doing"}])).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].status, PlanStatus::Pending);
        assert_eq!(items[1].status, PlanStatus::Doing);
    }

    #[test]
    fn sanitize_rejects_junk() {
        assert!(sanitize_items(&json!("不是数组")).is_err());
        assert!(sanitize_items(&json!([{"status": "done"}])).is_err(), "缺标题要报错");
        assert!(sanitize_items(&json!([1, 2])).is_err());
        let too_many: Vec<String> = (0..MAX_PLAN_ITEMS + 1).map(|i| format!("项 {i}")).collect();
        assert!(sanitize_items(&json!(too_many)).is_err(), "超上限要报错");
    }
}
