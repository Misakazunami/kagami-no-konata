use std::collections::BTreeMap;

use crate::llm::types::ToolCallDelta;

/// 一次已组装完成的工具调用
#[derive(Debug, Clone, PartialEq)]
pub struct AssembledCall {
    pub id: String,
    pub name: String,
    /// 解析后的参数（解析失败时为空对象）
    pub arguments: serde_json::Value,
    /// 模型给出的原始参数字符串（落库展示用）
    pub raw_arguments: String,
    /// JSON 解析失败的原因（交由 runner 回灌给模型让它重试）
    pub parse_error: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct Slot {
    id: Option<String>,
    name: String,
    arguments: String,
}

/// 工具调用分片累积器
///
/// OpenAI 与 DeepSeek 都按 `index` 分片推送：`id` 与 `name` 通常只在第一片出现，
/// `arguments` 是**逐字符拼接**的 JSON 片段。这里按 index 归并，流结束后
/// 一次性解析参数。
#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    slots: BTreeMap<usize, Slot>,
}

impl ToolCallAccumulator {
    pub fn push(&mut self, delta: ToolCallDelta) {
        let slot = self.slots.entry(delta.index).or_default();

        if let Some(id) = delta.id.filter(|s| !s.is_empty()) {
            slot.id = Some(id);
        }

        if let Some(name) = delta.name.filter(|s| !s.is_empty()) {
            if slot.name.is_empty() {
                slot.name = name;
            } else if slot.name != name && !slot.name.ends_with(&name) {
                // 少数网关会把函数名也分片推送；重复推送整名时忽略
                slot.name.push_str(&name);
            }
        }

        if let Some(args) = delta.arguments {
            slot.arguments.push_str(&args);
        }
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// 完成组装
    ///
    /// `stream_id` 用于给缺失 `id` 的网关生成稳定兜底的调用 ID。
    pub fn finish(self, stream_id: &str) -> Vec<AssembledCall> {
        self.slots
            .into_iter()
            .filter(|(_, slot)| !slot.name.trim().is_empty())
            .map(|(index, slot)| {
                let raw = slot.arguments.trim().to_string();
                let (arguments, parse_error) = if raw.is_empty() {
                    (serde_json::json!({}), None)
                } else {
                    match serde_json::from_str::<serde_json::Value>(&raw) {
                        Ok(value) if value.is_object() => (value, None),
                        Ok(_) => (
                            serde_json::json!({}),
                            Some("参数必须是 JSON 对象".to_string()),
                        ),
                        Err(e) => (serde_json::json!({}), Some(format!("参数不是合法 JSON：{}", e))),
                    }
                };
                AssembledCall {
                    id: slot
                        .id
                        .unwrap_or_else(|| format!("call_{}_{}", stream_id, index)),
                    name: slot.name,
                    arguments,
                    raw_arguments: raw,
                    parse_error,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(index: usize, id: Option<&str>, name: Option<&str>, args: Option<&str>) -> ToolCallDelta {
        ToolCallDelta {
            index,
            id: id.map(|s| s.to_string()),
            name: name.map(|s| s.to_string()),
            arguments: args.map(|s| s.to_string()),
        }
    }

    #[test]
    fn accumulates_fragmented_arguments() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(delta(0, Some("c1"), Some("read_file"), Some("{\"pa")));
        acc.push(delta(0, None, None, Some("th\":\"a.")));
        acc.push(delta(0, None, None, Some("txt\"}")));

        let calls = acc.finish("s1");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "c1");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], "a.txt");
        assert!(calls[0].parse_error.is_none());
    }

    #[test]
    fn keeps_parallel_calls_in_index_order() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(delta(1, Some("c2"), Some("list_dir"), Some("{}")));
        acc.push(delta(0, Some("c1"), Some("read_file"), Some("{}")));

        let calls = acc.finish("s1");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[1].name, "list_dir");
    }

    #[test]
    fn empty_arguments_become_empty_object() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(delta(0, Some("c1"), Some("get_current_time"), None));
        let calls = acc.finish("s1");
        assert_eq!(calls[0].arguments, serde_json::json!({}));
        assert!(calls[0].parse_error.is_none());
    }

    #[test]
    fn invalid_json_is_reported_not_panicked() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(delta(0, Some("c1"), Some("read_file"), Some("{\"path\":")));
        let calls = acc.finish("s1");
        assert!(calls[0].parse_error.is_some());
        assert_eq!(calls[0].arguments, serde_json::json!({}));
    }

    #[test]
    fn non_object_json_is_rejected() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(delta(0, Some("c1"), Some("read_file"), Some("[1,2,3]")));
        let calls = acc.finish("s1");
        assert!(calls[0].parse_error.is_some());
    }

    #[test]
    fn missing_id_gets_stable_fallback() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(delta(0, None, Some("get_current_time"), Some("{}")));
        let calls = acc.finish("stream-9");
        assert_eq!(calls[0].id, "call_stream-9_0");
    }

    #[test]
    fn repeated_full_name_is_not_duplicated() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(delta(0, Some("c1"), Some("read_file"), Some("{")));
        acc.push(delta(0, None, Some("read_file"), Some("}")));
        let calls = acc.finish("s1");
        assert_eq!(calls[0].name, "read_file");
    }

    #[test]
    fn gap_filling_name_fragments_are_joined() {
        // 少数网关把名字分片推送：先 "read_" 再 "file"
        let mut acc = ToolCallAccumulator::default();
        acc.push(delta(0, Some("c1"), Some("read_"), Some("{")));
        acc.push(delta(0, None, Some("file"), Some("}")));
        let calls = acc.finish("s1");
        assert_eq!(calls[0].name, "read_file");
    }

    #[test]
    fn nameless_slots_are_dropped() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(delta(0, Some("c1"), None, Some("{}")));
        assert!(acc.finish("s1").is_empty());
    }
}
