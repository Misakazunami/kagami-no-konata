use anyhow::Result;
use serde_json::{json, Value};

use crate::agent::plan::{sanitize_items, SessionPlan};
use crate::agent::harness::traits::{
    Permission, Tool, ToolCtx, ToolDescriptor, ToolOutput, EVENT_PLAN_UPDATED,
};

/// 计划项条数上限（与 store 的 `MAX_PLAN_ITEMS` 一致，见 `agent::plan`）
pub const MAX_ITEMS: usize = crate::store::chat_store::MAX_PLAN_ITEMS;

/// 维护当前会话的任务计划
///
/// 为什么值得单独做一个工具：工具结果不跨轮保留，长任务的进度必须落在
/// **会话级状态**里才不会丢。计划由模型自己写、每次都注入 system prompt，
/// 同时通过 `plan-updated` 事件同步到界面，所以模型和用户看的是同一份进度。
///
/// 权限是 `WriteApp`（只写应用自己的数据库，不外发、不碰工作区）：
/// 因此**不需要审批**——否则每更新一次进度就弹一次窗，用户会被逼疯。
pub struct UpdatePlan;

#[async_trait::async_trait]
impl Tool for UpdatePlan {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "update_plan",
            "更新任务计划",
            "把当前任务的执行计划写下来（用户也能看到同一份进度）。任务超过两步时先写下计划，每完成一步就更新对应项的状态；被卡住或用户中途停止时把剩余项标成 blocked。items 传空数组表示计划已全部结束、清空计划。整体覆盖式写入（每次提交完整列表）。",
            Permission::WriteApp,
            json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "maxItems": MAX_ITEMS,
                        "description": format!("完整计划列表（最多 {} 项，整体覆盖）", MAX_ITEMS),
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string", "description": "一句话描述这一步要做什么（≤200 字）" },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "doing", "done", "blocked"],
                                    "description": "pending 待办 / doing 进行中 / done 已完成 / blocked 做不下去"
                                }
                            },
                            "required": ["title", "status"],
                            "additionalProperties": false
                        }
                    },
                    "note": {
                        "type": "string",
                        "description": "可选的一行备注（例如等用户确认、缺依赖等）"
                    }
                },
                "required": ["items"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let raw = args.get("items").cloned().unwrap_or(Value::Array(Vec::new()));
        let items = sanitize_items(&raw).map_err(|e| anyhow::anyhow!(e))?;
        let note = args
            .get("note")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let plan = SessionPlan::new(items, note);

        // 写库：拿不到 store 时**必须报错**（不能让模型以为计划记下来了）
        let Some(store) = cx.services.chat_store.as_ref() else {
            anyhow::bail!("当前环境没有会话存储，无法保存计划");
        };
        {
            let store = store
                .lock()
                .map_err(|e| anyhow::anyhow!("会话存储不可用：{}", e))?;
            store
                .save_plan(cx.session_id, &plan)
                .map_err(|e| anyhow::anyhow!("保存计划失败：{}", e))?;
        }

        // 同步界面（主窗口订阅；悬浮窗既不订阅工具事件也不显示计划面板）
        cx.emit.emit(
            EVENT_PLAN_UPDATED,
            json!({
                "session_id": cx.session_id,
                "items": plan.items,
                "note": plan.note,
            }),
        );

        let summary = plan.summary();
        let mut body = format!("计划已更新：{}\n", summary);
        if plan.items.is_empty() {
            body.push_str("（计划已清空）\n");
        } else {
            for (index, item) in plan.items.iter().enumerate() {
                body.push_str(&format!(
                    "{}. {} {}\n",
                    index + 1,
                    item.status.checkbox(),
                    item.title
                ));
            }
        }
        if let Some(note) = plan.note.as_deref() {
            body.push_str(&format!("备注：{}\n", note));
        }

        Ok(ToolOutput::text(body).with_preview(format!("计划 · {}", summary)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness::jail::WorkspaceSet;
    use crate::agent::harness::traits::{EventSink, ToolLimits, ToolServices};
    use crate::agent::plan::PlanStatus;
    use crate::config::types::{ToolConfig, ToolMode};
    use crate::store::chat_store::ChatStore;
    use crate::store::db;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// 记录事件的 sink
    struct RecordingSink {
        events: Mutex<Vec<(String, Value)>>,
    }

    impl RecordingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(Vec::new()),
            })
        }
        fn payloads(&self, name: &str) -> Vec<Value> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, p)| p.clone())
                .collect()
        }
    }

    impl EventSink for RecordingSink {
        fn emit(&self, event: &str, payload: Value) {
            self.events
                .lock()
                .unwrap()
                .push((event.to_string(), payload));
        }
    }

    struct Fixture {
        dir: std::path::PathBuf,
        services: ToolServices,
        sink: Arc<RecordingSink>,
        cancel: Arc<AtomicBool>,
        store: Arc<Mutex<ChatStore>>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("konata-plan-{}-{}", tag, uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let conn = db::init_db(&dir).unwrap();
            let store = Arc::new(Mutex::new(ChatStore::new(conn)));
            // 计划有外键指向 sessions，先建一条会话
            store.lock().unwrap().create_session("konata-default", "测试", None, None, None).unwrap();
            let cfg = ToolConfig::with_single_root(&dir, true, "测试");
            let set = WorkspaceSet::from_config(&cfg, &dir);
            let mut services = ToolServices::minimal(dir.clone(), set, ToolMode::Standard);
            services.chat_store = Some(store.clone());
            Self {
                dir,
                services,
                sink: RecordingSink::new(),
                cancel: Arc::new(AtomicBool::new(false)),
                store,
            }
        }

        fn ctx(&self) -> ToolCtx<'_> {
            ToolCtx {
                session_id: "s1",
                stream_id: "st1",
                call_id: "c1",
                step: 0,
                cancel: self.cancel.clone(),
                services: &self.services,
                limits: ToolLimits {
                    max_output_bytes: 64 * 1024,
                    call_timeout: Duration::from_secs(5),
                    approval_timeout: Duration::from_secs(5),
                },
                emit: self.sink.clone(),
                approver: Arc::new(crate::agent::harness::traits::DenyAllApprover),
            }
        }

        /// 计划外键指向的会话 id
        fn session_id(&self) -> String {
            self.store
                .lock()
                .unwrap()
                .list_sessions()
                .unwrap()
                .first()
                .unwrap()
                .id
                .clone()
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
    fn writes_plan_and_emits_event() {
        let fx = Fixture::new("write");
        let session_id = fx.session_id();
        let cx = ToolCtx {
            session_id: &session_id,
            ..fx.ctx()
        };
        let out = block_on(UpdatePlan.call(
            json!({"items": [
                {"title": "读代码", "status": "done"},
                {"title": "改实现", "status": "doing"}
            ], "note": "等用户确认"}),
            &cx,
        ))
        .unwrap();

        assert!(out.content.contains("共 2 项"), "{}", out.content);
        assert!(out.content.contains("[x] 读代码"), "{}", out.content);
        assert!(out.preview.unwrap_or_default().contains("计划"));

        // 落库可回读
        let stored = fx
            .store
            .lock()
            .unwrap()
            .get_plan(&session_id)
            .unwrap()
            .expect("计划应当落库");
        assert_eq!(stored.items.len(), 2);
        assert_eq!(stored.items[0].status, PlanStatus::Done);
        assert_eq!(stored.note.as_deref(), Some("等用户确认"));

        // 事件带会话与完整列表（前端据此渲染进度面板）
        let events = fx.sink.payloads(EVENT_PLAN_UPDATED);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["session_id"], session_id);
        assert_eq!(events[0]["items"].as_array().unwrap().len(), 2);
        assert_eq!(events[0]["note"], "等用户确认");
    }

    #[test]
    fn empty_items_clears_the_plan() {
        let fx = Fixture::new("clear");
        let session_id = fx.session_id();
        let cx = ToolCtx {
            session_id: &session_id,
            ..fx.ctx()
        };
        block_on(UpdatePlan.call(json!({"items": ["唯一一项"]}), &cx)).unwrap();
        assert!(fx.store.lock().unwrap().get_plan(&session_id).unwrap().is_some());

        let out = block_on(UpdatePlan.call(json!({"items": []}), &cx)).unwrap();
        assert!(out.content.contains("计划已清空") || out.content.contains("计划已更新"), "{}", out.content);
        assert!(
            fx.store.lock().unwrap().get_plan(&session_id).unwrap().is_none(),
            "空计划必须删除记录，而不是留下空壳"
        );
    }

    #[test]
    fn rejects_invalid_items_without_touching_the_plan() {
        let fx = Fixture::new("invalid");
        let session_id = fx.session_id();
        let cx = ToolCtx {
            session_id: &session_id,
            ..fx.ctx()
        };
        block_on(UpdatePlan.call(json!({"items": ["有效一项"]}), &cx)).unwrap();

        let err = block_on(UpdatePlan.call(json!({"items": [{"status": "done"}]}), &cx)).unwrap_err();
        assert!(err.to_string().contains("title"), "{err}");
        let stored = fx.store.lock().unwrap().get_plan(&session_id).unwrap().unwrap();
        assert_eq!(stored.items.len(), 1, "校验失败不得改动已有计划");
    }

    #[test]
    fn is_write_app_permission_and_needs_no_approval() {
        let descriptor = UpdatePlan.descriptor();
        assert_eq!(descriptor.permission, Permission::WriteApp);
        assert!(!descriptor.permission.requires_approval());
        // 只读模式下不可见（只读模式本就不该有副作用）
        assert!(!descriptor.permission.visible_in(ToolMode::ReadOnly));
    }
}
