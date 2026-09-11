use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;

use super::traits::{ApprovalRequest, Approver, ToolDecision};

/// 等待用户决定的审批项
pub struct PendingApproval {
    pub session_id: String,
    pub stream_id: String,
    pub tool: String,
    pub sender: oneshot::Sender<ToolDecision>,
}

/// 进行中的审批（key = approval_id）
pub type ApprovalMap = Arc<Mutex<HashMap<String, PendingApproval>>>;

pub fn new_approval_map() -> ApprovalMap {
    Arc::new(Mutex::new(HashMap::new()))
}

/// 把某个生成任务上所有等待中的审批按"拒绝"结束
///
/// `stop_generation` 必须调用它：否则用户点了停止，生成仍会卡在审批等待里
/// 直到超时（默认 120 秒）。
pub fn cancel_pending_for_stream(map: &ApprovalMap, stream_id: &str) -> usize {
    let ids: Vec<String> = {
        let guard = map.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .iter()
            .filter(|(_, p)| p.stream_id == stream_id)
            .map(|(id, _)| id.clone())
            .collect()
    };
    let mut cancelled = 0;
    for id in ids {
        let entry = {
            let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
            guard.remove(&id)
        };
        if let Some(pending) = entry {
            let _ = pending.sender.send(ToolDecision::Deny);
            cancelled += 1;
        }
    }
    cancelled
}

/// 基于 Tauri 事件的审批通道（主窗口）
pub struct TauriApprover {
    app: AppHandle,
    pending: ApprovalMap,
}

impl TauriApprover {
    pub fn new(app: AppHandle, pending: ApprovalMap) -> Self {
        Self { app, pending }
    }
}

#[async_trait::async_trait]
impl Approver for TauriApprover {
    async fn request(&self, req: ApprovalRequest) -> ToolDecision {
        let approval_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        let session_id = req.session_id.clone();
        let stream_id = req.stream_id.clone();

        {
            let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(
                approval_id.clone(),
                PendingApproval {
                    session_id: session_id.clone(),
                    stream_id: stream_id.clone(),
                    tool: req.tool.clone(),
                    sender: tx,
                },
            );
        }

        let expires_at = chrono::Local::now()
            + chrono::Duration::from_std(req.timeout)
                .unwrap_or_else(|_| chrono::Duration::seconds(120));

        // 只发给主窗口：审批 UI 只存在于主窗口
        let target = tauri::EventTarget::webview_window("main");
        let _ = self.app.emit_to(
            target.clone(),
            "tool-approval-request",
            json!({
                "session_id": session_id,
                "stream_id": stream_id,
                "approval_id": approval_id,
                "call_id": req.call_id,
                "tool": req.tool,
                "tool_label": req.tool_label,
                "args": req.args,
                "permission": req.permission.as_str(),
                "risk": req.permission.risk_label(),
                "expires_at": expires_at.to_rfc3339(),
            }),
        );

        // 超时 / 通道断开（前端没应答、审批被取消）一律按拒绝处理
        let decision = match tokio::time::timeout(req.timeout, rx).await {
            Ok(Ok(decision)) => decision,
            Ok(Err(_)) => ToolDecision::Deny,
            Err(_) => ToolDecision::Deny,
        };

        {
            let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            map.remove(&approval_id);
        }

        let _ = self.app.emit_to(
            target,
            "tool-approval-resolved",
            json!({
                "session_id": session_id,
                "stream_id": stream_id,
                "approval_id": approval_id,
                "decision": decision.as_str(),
            }),
        );

        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_pending_denies_only_matching_stream() {
        let map = new_approval_map();
        let (tx1, mut rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();
        {
            let mut guard = map.lock().unwrap();
            guard.insert(
                "a1".to_string(),
                PendingApproval {
                    session_id: "s1".to_string(),
                    stream_id: "stream-1".to_string(),
                    tool: "write_file".to_string(),
                    sender: tx1,
                },
            );
            guard.insert(
                "a2".to_string(),
                PendingApproval {
                    session_id: "s1".to_string(),
                    stream_id: "stream-2".to_string(),
                    tool: "run_command".to_string(),
                    sender: tx2,
                },
            );
        }

        assert_eq!(cancel_pending_for_stream(&map, "stream-1"), 1);
        assert_eq!(rx1.try_recv().unwrap(), ToolDecision::Deny);
        assert_eq!(map.lock().unwrap().len(), 1, "另一个 stream 的审批不应被清理");

        // 清理不存在的 stream 是安全的空操作
        assert_eq!(cancel_pending_for_stream(&map, "nope"), 0);
    }
}
