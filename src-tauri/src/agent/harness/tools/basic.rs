use anyhow::Result;
use serde_json::{json, Value};

use crate::agent::harness::traits::{Permission, Tool, ToolCtx, ToolDescriptor, ToolOutput};


/// 当前本地时间
///
/// 取代了 `SystemAgent` 原先"只能看时间"的硬编码分支：同一个能力现在
/// 既能被 `/sys 几点了` 触发，也能由模型自主调用。
pub struct GetCurrentTime;

#[async_trait::async_trait]
impl Tool for GetCurrentTime {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "get_current_time",
            "查看时间",
            "获取当前本地日期、时间与星期。用户询问时间、日期、今天星期几时使用。",
            Permission::Read,
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, _args: Value, _cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let now = chrono::Local::now();
        let weekday = match chrono::Datelike::weekday(&now) {
            chrono::Weekday::Mon => "星期一",
            chrono::Weekday::Tue => "星期二",
            chrono::Weekday::Wed => "星期三",
            chrono::Weekday::Thu => "星期四",
            chrono::Weekday::Fri => "星期五",
            chrono::Weekday::Sat => "星期六",
            chrono::Weekday::Sun => "星期日",
        };
        let text = format!(
            "当前本地时间：{} {}（{}）",
            now.format("%Y-%m-%d"),
            now.format("%H:%M:%S"),
            weekday
        );
        Ok(ToolOutput::text(text.clone()).with_preview(text))
    }
}

/// 系统与应用环境信息
pub struct GetSystemInfo;

#[async_trait::async_trait]
impl Tool for GetSystemInfo {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "get_system_info",
            "系统信息",
            "获取操作系统、CPU 核心数、可用工作区列表等本机信息。用户询问系统状态、环境或工作区位置时使用。",
            Permission::Read,
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, _args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get().to_string())
            .unwrap_or_else(|_| "未知".to_string());

        let roots = cx
            .services
            .workspaces
            .list()
            .iter()
            .map(|r| {
                format!(
                    "{}（{}，{}，{}）",
                    r.id,
                    r.path,
                    if r.writable { "可写" } else { "只读" },
                    if r.available { "可用" } else { "已失效" }
                )
            })
            .collect::<Vec<_>>()
            .join("\n  ");

        let text = format!(
            "操作系统：{} {}\n架构：{}\nCPU 逻辑核心数：{}\n应用版本：{}\n当前工具模式：{:?}\n工作区：\n  {}",
            std::env::consts::OS,
            std::env::consts::FAMILY,
            std::env::consts::ARCH,
            cores,
            env!("CARGO_PKG_VERSION"),
            cx.services.mode,
            roots,
        );
        Ok(ToolOutput::text(text))
    }
}

/// 应用自身的状态（会话数、记忆数、用量）
pub struct GetAppStatus;

#[async_trait::async_trait]
impl Tool for GetAppStatus {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "get_app_status",
            "应用状态",
            "查看应用运行状态：会话数量、当前会话消息数、长期记忆条数与累计用量统计。",
            Permission::Read,
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, _args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let mut lines: Vec<String> = Vec::new();

        if let Some(store) = &cx.services.chat_store {
            let store = store.lock().unwrap_or_else(|e| e.into_inner());
            match store.list_sessions() {
                Ok(sessions) => {
                    lines.push(format!("会话总数：{}", sessions.len()));
                    if let Some(current) = sessions.iter().find(|s| s.id == cx.session_id) {
                        lines.push(format!("当前会话标题：{}", current.title));
                    }
                }
                Err(e) => lines.push(format!("会话列表读取失败：{}", e)),
            }
            match store.count_messages(cx.session_id) {
                Ok(count) => lines.push(format!("当前会话消息数：{}", count)),
                Err(e) => lines.push(format!("消息数读取失败：{}", e)),
            }
            if let Ok(stats) = store.get_usage_stats() {
                lines.push(format!(
                    "累计用量：{} 次请求 / 共 {} tokens（输入 {} / 输出 {}）",
                    stats.total_requests, stats.total_tokens, stats.prompt_tokens,
                    stats.completion_tokens
                ));
            }
        } else {
            lines.push("会话存储不可用".to_string());
        }

        if let Some(memory) = &cx.services.memory {
            let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
            // 只取条数：不要 list_memories——它会把全部向量反序列化一遍再丢掉
            match memory.count_memories() {
                Ok(count) => lines.push(format!("长期记忆条数：{}", count)),
                Err(e) => lines.push(format!("记忆读取失败：{}", e)),
            }
        }

        Ok(ToolOutput::text(lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness::jail::WorkspaceSet;
    use crate::agent::harness::traits::{NullSink, ToolLimits, ToolServices};
    use crate::config::types::{ToolConfig, ToolMode};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;

    fn ctx_fixture() -> (std::path::PathBuf, ToolServices, Arc<NullSink>) {
        let dir = std::env::temp_dir().join(format!("konata-basic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = ToolConfig::with_single_root(
            &dir,
            true,
            "测试",
        );
        let set = WorkspaceSet::from_config(&cfg, &dir);
        let services = ToolServices::minimal(dir.clone(), set, ToolMode::Standard);
        (dir, services, Arc::new(NullSink))
    }

    fn make_ctx<'a>(
        services: &'a ToolServices,
        sink: &'a Arc<NullSink>,
        cancel: &'a Arc<AtomicBool>,
    ) -> ToolCtx<'a> {
        ToolCtx {
            session_id: "s1",
            stream_id: "st1",
            call_id: "c1",
            step: 0,
            cancel: cancel.clone(),
            services,
            limits: ToolLimits {
                max_output_bytes: 64 * 1024,
                call_timeout: Duration::from_secs(5),
                approval_timeout: Duration::from_secs(5),
            },
            emit: sink.clone(),
            approver: Arc::new(crate::agent::harness::traits::DenyAllApprover),
        }
    }

    #[test]
    fn current_time_tool_reports_time() {
        let (dir, services, sink) = ctx_fixture();
        let cancel = Arc::new(AtomicBool::new(false));
        let cx = make_ctx(&services, &sink, &cancel);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = runtime
            .block_on(GetCurrentTime.call(json!({}), &cx))
            .unwrap();
        assert!(out.content.contains("当前本地时间"));
        assert!(out.content.contains("星期"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn system_info_lists_workspaces() {
        let (dir, services, sink) = ctx_fixture();
        let cancel = Arc::new(AtomicBool::new(false));
        let cx = make_ctx(&services, &sink, &cancel);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = runtime
            .block_on(GetSystemInfo.call(json!({}), &cx))
            .unwrap();
        assert!(out.content.contains("工作区"));
        assert!(out.content.contains("default"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
