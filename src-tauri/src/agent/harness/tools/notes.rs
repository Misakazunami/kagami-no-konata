use anyhow::Result;
use serde_json::{json, Value};

use crate::agent::harness::traits::{
    Permission, Tool, ToolCtx, ToolDescriptor, ToolOutput,
};
use crate::agent::notes::{
    sanitize_note, SessionNote, MAX_NOTES, MAX_NOTE_BYTES, MAX_TOTAL_BYTES,
};

/// 记下一条跨轮有效的工作记忆
///
/// 为什么值得做：工具结果刻意不跨轮保留，长任务因此每轮都要重读同样的文件；
/// 计划（`update_plan`）解决"做到哪一步"，这里解决"已经查明了什么"。
///
/// 权限是 `WriteSession`（只写应用自己的会话数据），**不需要审批**——否则每记一条就弹窗；
/// 只读模式下同样可见（Plan 阶段调查出的结论要能跨轮保留）。
/// 内容与条数都有硬上限，注入时统一带 `untrusted` 标记（见 `agent::notes`）。
pub struct SaveNote;

#[async_trait::async_trait]
impl Tool for SaveNote {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "save_note",
            "记住结论",
            format!(
                "把已经查明的结论记进本会话的工作记忆，供之后几轮直接使用（避免重复调查）。只记**结论**：例如「认证在 auth.rs:42，token 过期走刷新分支」。不要复制大段原文、不要记时间敏感信息；每条上限 {} 字节，最多 {} 条（超出会淘汰最旧的），总量上限 {} KB。",
                MAX_NOTE_BYTES,
                MAX_NOTES,
                MAX_TOTAL_BYTES / 1024
            ),
            Permission::WriteSession,
            json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "要记住的结论（简洁、可复用）" },
                    "title": { "type": "string", "description": "可选短标题（≤60 字），列表展示用" }
                },
                "required": ["content"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        if !cx.services.working_memory {
            anyhow::bail!("工作记忆已在设置里关闭");
        }
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("缺少必填参数「content」（字符串）"))?;
        let title = args.get("title").and_then(|v| v.as_str());
        let (title, content) = sanitize_note(title, content).map_err(|e| anyhow::anyhow!(e))?;

        let Some(store) = cx.services.chat_store.as_ref() else {
            anyhow::bail!("当前环境没有会话存储，无法保存工作记忆");
        };
        let note = SessionNote::new(uuid::Uuid::new_v4().to_string(), title, content);
        let remaining = {
            let store = store
                .lock()
                .map_err(|e| anyhow::anyhow!("会话存储不可用：{}", e))?;
            store
                .save_note(cx.session_id, &note)
                .map_err(|e| anyhow::anyhow!("保存工作记忆失败：{}", e))?;
            store.list_notes(cx.session_id).unwrap_or_default().len()
        };

        cx.emit.emit(
            crate::agent::harness::EVENT_NOTES_UPDATED,
            json!({
                "session_id": cx.session_id,
                "count": remaining,
            }),
        );

        let body = format!(
            "已记住（当前 {} 条）：[{}] {}",
            remaining,
            note.label(),
            note.content
        );
        Ok(ToolOutput::text(body.clone())
            .with_preview(format!("工作记忆 {} 条 · {}", remaining, note.label())))
    }
}

/// 忘掉一条（或全部）工作记忆
pub struct ForgetNote;

#[async_trait::async_trait]
impl Tool for ForgetNote {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "forget_note",
            "忘掉结论",
            "从工作记忆里删除一条已经过时或错误的结论；不传 id 则清空本会话的全部工作记忆。",
            Permission::WriteSession,
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "要删除的笔记 id；省略表示清空全部" }
                },
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        if !cx.services.working_memory {
            anyhow::bail!("工作记忆已在设置里关闭");
        }
        let Some(store) = cx.services.chat_store.as_ref() else {
            anyhow::bail!("当前环境没有会话存储");
        };
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // 删除前先确认 id 存在：静默"删了 0 条"会让模型以为记错的是别的东西
        let removed = {
            let store = store.lock().map_err(|e| anyhow::anyhow!("{}", e))?;
            if let Some(id) = id.as_deref() {
                let exists = store
                    .list_notes(cx.session_id)
                    .unwrap_or_default()
                    .iter()
                    .any(|note| note.id == id);
                if !exists {
                    anyhow::bail!("没有找到 id 为 {} 的工作记忆", id);
                }
            }
            store
                .delete_note(cx.session_id, id.as_deref())
                .map_err(|e| anyhow::anyhow!("删除工作记忆失败：{}", e))?
        };

        cx.emit.emit(
            crate::agent::harness::EVENT_NOTES_UPDATED,
            json!({
                "session_id": cx.session_id,
                "count": 0,
                "removed": removed,
            }),
        );

        let body = if id.is_some() {
            format!("已删除 1 条工作记忆（本次共删除 {} 条）", removed)
        } else {
            format!("已清空工作记忆（共删除 {} 条）", removed)
        };
        Ok(ToolOutput::text(body.clone()).with_preview(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness::jail::WorkspaceSet;
    use crate::agent::harness::traits::{
        DenyAllApprover, EventSink, ToolLimits, ToolServices, EVENT_NOTES_UPDATED,
    };
    use crate::agent::notes::MAX_NOTES;
    use crate::config::types::{ToolConfig, ToolMode};
    use crate::store::chat_store::ChatStore;
    use crate::store::db;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<(String, Value)>>,
    }

    impl EventSink for Recorder {
        fn emit(&self, event: &str, payload: Value) {
            self.events
                .lock()
                .unwrap()
                .push((event.to_string(), payload));
        }
    }

    struct Fixture {
        dir: PathBuf,
        services: ToolServices,
        sink: Arc<Recorder>,
        store: Arc<Mutex<ChatStore>>,
        session_id: String,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("konata-note-{}-{}", tag, uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let conn = db::init_db(&dir).unwrap();
            let store = Arc::new(Mutex::new(ChatStore::new(conn)));
            let session = store
                .lock()
                .unwrap()
                .create_session("konata-default", "测试", None, None, None)
                .unwrap();
            let cfg = ToolConfig::with_single_root(&dir, true, "测试");
            let set = WorkspaceSet::from_config(&cfg, &dir);
            let mut services = ToolServices::minimal(dir.clone(), set, ToolMode::Standard);
            services.chat_store = Some(store.clone());
            services.working_memory = true;
            Self {
                dir,
                services,
                sink: Arc::new(Recorder::default()),
                store,
                session_id: session.id,
            }
        }

        fn ctx(&self) -> ToolCtx<'_> {
            ToolCtx {
                session_id: &self.session_id,
                stream_id: "st1",
                call_id: "c1",
                step: 0,
                cancel: Arc::new(AtomicBool::new(false)),
                services: &self.services,
                limits: ToolLimits {
                    max_output_bytes: 64 * 1024,
                    call_timeout: Duration::from_secs(5),
                    approval_timeout: Duration::from_secs(5),
                },
                emit: self.sink.clone(),
                approver: Arc::new(DenyAllApprover),
            }
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn saves_a_note_and_emits_event() {
        let fx = Fixture::new("save");
        let cx = fx.ctx();
        let out = block_on(SaveNote.call(
            json!({"content": "认证在 auth.rs:42", "title": "认证"}),
            &cx,
        ))
        .unwrap();
        assert!(out.content.contains("已记住"), "{}", out.content);
        assert!(out.content.contains("auth.rs:42"), "{}", out.content);

        let notes = fx.store.lock().unwrap().list_notes(&fx.session_id).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title.as_deref(), Some("认证"));

        let events = fx.sink.events.lock().unwrap();
        let updated: Vec<_> = events
            .iter()
            .filter(|(name, _)| name == EVENT_NOTES_UPDATED)
            .collect();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].1["session_id"], fx.session_id);
        assert_eq!(updated[0].1["count"], 1);
    }

    #[test]
    fn rejects_oversized_or_empty_content() {
        let fx = Fixture::new("limits");
        let cx = fx.ctx();
        let err = block_on(SaveNote.call(json!({"content": "   "}), &cx)).unwrap_err();
        assert!(err.to_string().contains("不能为空"), "{err}");

        let err = block_on(SaveNote.call(
            json!({"content": "x".repeat(MAX_NOTE_BYTES + 1)}),
            &cx,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("过长"), "{err}");
        assert!(fx.store.lock().unwrap().list_notes(&fx.session_id).unwrap().is_empty());
    }

    #[test]
    fn oldest_notes_are_evicted_by_count() {
        let fx = Fixture::new("evict");
        let cx = fx.ctx();
        for index in 0..MAX_NOTES + 3 {
            block_on(SaveNote.call(json!({"content": format!("结论 {}", index)}), &cx)).unwrap();
        }
        let notes = fx.store.lock().unwrap().list_notes(&fx.session_id).unwrap();
        assert_eq!(notes.len(), MAX_NOTES, "条数上限必须生效");
        // 最旧的被淘汰，最新的一定还在
        assert!(!notes.iter().any(|note| note.content == "结论 0"));
        assert!(notes
            .iter()
            .any(|note| note.content == format!("结论 {}", MAX_NOTES + 2)));
    }

    #[test]
    fn forget_note_deletes_one_or_all() {
        let fx = Fixture::new("forget");
        let cx = fx.ctx();
        block_on(SaveNote.call(json!({"content": "A"}), &cx)).unwrap();
        block_on(SaveNote.call(json!({"content": "B"}), &cx)).unwrap();
        let notes = fx.store.lock().unwrap().list_notes(&fx.session_id).unwrap();
        let id = notes[0].id.clone();

        let out = block_on(ForgetNote.call(json!({"id": id}), &cx)).unwrap();
        assert!(out.content.contains("已删除 1 条"), "{}", out.content);
        assert_eq!(fx.store.lock().unwrap().list_notes(&fx.session_id).unwrap().len(), 1);

        // 不存在的 id 必须报错，而不是静默显示"删了 0 条"
        let err = block_on(ForgetNote.call(json!({"id": "ghost"}), &cx)).unwrap_err();
        assert!(err.to_string().contains("没有找到"), "{err}");

        let out = block_on(ForgetNote.call(json!({}), &cx)).unwrap();
        assert!(out.content.contains("已清空"), "{}", out.content);
        assert!(fx.store.lock().unwrap().list_notes(&fx.session_id).unwrap().is_empty());
    }

    #[test]
    fn disabled_feature_refuses_with_a_clear_message() {
        let mut fx = Fixture::new("disabled");
        fx.services.working_memory = false;
        let cx = fx.ctx();
        let err = block_on(SaveNote.call(json!({"content": "x"}), &cx)).unwrap_err();
        assert!(err.to_string().contains("已在设置里关闭"), "{err}");
        let err = block_on(ForgetNote.call(json!({}), &cx)).unwrap_err();
        assert!(err.to_string().contains("已在设置里关闭"), "{err}");
    }

    #[test]
    fn tools_need_no_approval_and_are_visible_in_read_only() {
        for descriptor in [SaveNote.descriptor(), ForgetNote.descriptor()] {
            assert_eq!(descriptor.permission, Permission::WriteSession);
            assert!(!descriptor.permission.requires_approval(), "写会话内数据不该弹窗");
            // Plan 模式（只读）要能把调查结论跨轮保留下来
            assert!(descriptor.permission.visible_in(ToolMode::ReadOnly));
            assert!(descriptor.permission.visible_in(ToolMode::Standard));
        }
    }
}
