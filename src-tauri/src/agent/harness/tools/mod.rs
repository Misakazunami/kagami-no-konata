pub mod basic;
pub mod fs_read;
pub mod fs_write;
pub mod memory;
pub mod shell;
pub mod web;

use std::sync::Arc;

use super::registry::ToolRegistry;
use super::traits::Tool;

/// 内置工具注册表
///
/// 新增工具只需实现 `Tool` 并在此登记；模式过滤、审批、事件、截断
/// 全部由注册表与 runner 统一处理，工具实现本身不关心这些。
pub fn builtin_registry() -> ToolRegistry {
    ToolRegistry::new(vec![
        // 只读：文件系统
        Arc::new(fs_read::ReadFile) as Arc<dyn Tool>,
        Arc::new(fs_read::ListDir),
        Arc::new(fs_read::GlobSearch),
        Arc::new(fs_read::GrepSearch),
        // 只读：本机与应用状态
        Arc::new(basic::GetCurrentTime),
        Arc::new(basic::GetSystemInfo),
        Arc::new(basic::GetAppStatus),
        // 只读：记忆与人设
        Arc::new(memory::SearchMemory),
        Arc::new(memory::ListPersonas),
        Arc::new(memory::ReadPersona),
        // 写应用数据（免审批）
        Arc::new(memory::SaveMemory),
        // 写工作区（需审批）
        Arc::new(fs_write::WriteFile),
        Arc::new(fs_write::EditFile),
        // 执行命令（需审批，仅 Full 模式可见，敏感命令硬拦截）
        Arc::new(shell::RunCommand),
        // 网络（需审批 + 域名白名单）
        Arc::new(web::WebFetch),
        // 调起系统默认程序（需审批）
        Arc::new(web::OpenWithSystem),
    ])
}

/// 参数解析辅助：所有工具都必须经过这些函数读取参数，
/// 缺失或类型错误一律返回可读的中文错误（会回灌给模型自我修正）。
pub mod args {
    use anyhow::{anyhow, Result};
    use serde_json::Value;

    pub fn required_str(args: &Value, key: &str) -> Result<String> {
        let value = args
            .get(key)
            .ok_or_else(|| anyhow!("缺少必填参数「{}」", key))?;
        match value.as_str() {
            Some(text) if !text.trim().is_empty() => Ok(text.to_string()),
            Some(_) => Err(anyhow!("参数「{}」不能为空", key)),
            None => Err(anyhow!("参数「{}」必须是字符串", key)),
        }
    }

    pub fn optional_str(args: &Value, key: &str) -> Option<String> {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
    }

    pub fn optional_bool(args: &Value, key: &str, default: bool) -> bool {
        args.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
    }

    /// 读取整数参数并夹到 `[min, max]`
    pub fn bounded_usize(args: &Value, key: &str, default: usize, min: usize, max: usize) -> usize {
        args.get(key)
            .and_then(|v| v.as_u64())
            .map(|v| (v as usize).clamp(min, max))
            .unwrap_or(default)
    }

    /// 读取浮点参数并夹到 `[min, max]`
    pub fn bounded_f32(args: &Value, key: &str, default: f32, min: f32, max: f32) -> f32 {
        args.get(key)
            .and_then(|v| v.as_f64())
            .map(|v| (v as f32).clamp(min, max))
            .unwrap_or(default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::ToolMode;

    #[test]
    fn builtin_registry_has_unique_names() {
        let registry = builtin_registry();
        // len 与 schema 数一致即说明没有重复名覆盖
        assert_eq!(registry.len(), 16, "内置工具数量应当与设计一致");
        let names = registry.visible_names(ToolMode::Full);
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "工具名不得重复");
    }

    #[test]
    fn execute_tools_only_visible_in_full_mode() {
        let registry = builtin_registry();
        assert!(registry.find("run_command", ToolMode::Standard).is_none());
        assert!(registry.find("run_command", ToolMode::Full).is_some());
    }

    #[test]
    fn readonly_mode_only_exposes_read_tools() {
        let registry = builtin_registry();
        for name in registry.visible_names(ToolMode::ReadOnly) {
            let permission = registry.permission_of(&name).unwrap();
            assert!(
                permission.is_read_only(),
                "只读模式不应暴露 {}（{:?}）",
                name,
                permission
            );
        }
    }

    #[test]
    fn tool_descriptions_are_persona_agnostic() {
        // 工具描述是模型可见的提示词，写死角色名会在用户切换人格后串戏
        for info in builtin_registry().infos(ToolMode::Full) {
            for text in [info.label.as_str(), info.description.as_str(), info.name.as_str()] {
                for name in ["此方", "こなた", "泉此方"] {
                    assert!(
                        !text.contains(name),
                        "工具 {} 的文案写死了角色名「{}」：{}",
                        info.name,
                        name,
                        text
                    );
                }
            }
        }
    }

    #[test]
    fn args_helpers_validate_types() {
        let args = serde_json::json!({"path": "a.txt", "n": 5, "flag": true});
        assert_eq!(args::required_str(&args, "path").unwrap(), "a.txt");
        assert!(args::required_str(&args, "missing").is_err());
        assert!(args::required_str(&args, "n").is_err());
        assert_eq!(args::bounded_usize(&args, "n", 1, 1, 3), 3);
        assert!(args::optional_bool(&args, "flag", false));
        assert_eq!(args::optional_str(&args, "missing"), None);
    }
}
