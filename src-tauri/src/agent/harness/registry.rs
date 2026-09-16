use std::collections::HashMap;
use std::sync::Arc;

use crate::config::types::ToolMode;
use crate::llm::types::ToolSchema;

use super::traits::{Permission, Tool, ToolDescriptor, ToolInfo};

/// 工具注册表
///
/// 唯一负责"哪些工具在当前模式下可见"的地方：不可见的工具既不出现在
/// 模型看到的 schema 里，也无法通过名称调用（模型幻觉出的名字会被拒绝）。
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    by_name: HashMap<&'static str, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        let mut by_name: HashMap<&'static str, Arc<dyn Tool>> = HashMap::new();
        for tool in &tools {
            let descriptor = tool.descriptor();
            if by_name.insert(descriptor.name, tool.clone()).is_some() {
                eprintln!("[harness] 工具名重复，后者覆盖前者：{}", descriptor.name);
            }
        }
        Self { tools, by_name }
    }

    /// 追加一批工具（MCP 等外部来源）
    ///
    /// 在构造期调用一次：注册表本身不做运行时变更，避免"工具集在会话中途变化"
    /// 这种会让模型与用户都困惑的状态。
    pub fn with_extra(mut self, extra: Vec<Arc<dyn Tool>>) -> Self {
        for tool in extra {
            let descriptor = tool.descriptor();
            if self.by_name.contains_key(descriptor.name) {
                eprintln!(
                    "[harness] 工具名与已有工具冲突，已跳过：{}",
                    descriptor.name
                );
                continue;
            }
            self.by_name.insert(descriptor.name, tool.clone());
            self.tools.push(tool);
        }
        self
    }

    /// 去掉若干工具后的注册表
    ///
    /// 用途：子代理的注册表必须排除 `spawn_subagents` 自己——它的权限是只读，
    /// 光靠模式过滤挡不住；运行时虽然也会拒绝（`services.subagent` 为 `None`），
    /// 但让模型看到"一个调了必然报错的工具"是纯粹的浪费。
    pub fn excluding(mut self, names: &[&str]) -> Self {
        self.tools
            .retain(|tool| !names.contains(&tool.descriptor().name));
        self.by_name
            .retain(|name, _| !names.contains(name));
        self
    }

    /// 空注册表：悬浮窗链路使用
    pub fn empty() -> Self {
        Self {
            tools: Vec::new(),
            by_name: HashMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// 按模式过滤后的可见工具描述
    pub fn visible(&self, mode: ToolMode) -> Vec<&Arc<dyn Tool>> {
        self.tools
            .iter()
            .filter(|t| t.enabled() && t.descriptor().permission.visible_in(mode))
            .collect()
    }

    /// 当前模式下的名字集合（用于识别模型幻觉出的工具名）
    pub fn visible_names(&self, mode: ToolMode) -> Vec<String> {
        self.visible(mode)
            .iter()
            .map(|t| t.descriptor().name.to_string())
            .collect()
    }

    /// 生成给模型的工具声明
    pub fn schemas(&self, mode: ToolMode) -> Vec<ToolSchema> {
        self.visible(mode)
            .iter()
            .map(|tool| {
                let d = tool.descriptor();
                ToolSchema::function(d.name, d.description.clone(), d.parameters.clone())
            })
            .collect()
    }

    /// 生成给前端的工具清单（不过滤模式，附带 enabled 标记）
    pub fn infos(&self, mode: ToolMode) -> Vec<ToolInfo> {
        let mut list: Vec<ToolInfo> = self
            .tools
            .iter()
            .map(|tool| {
                let d = tool.descriptor();
                ToolInfo {
                    name: d.name.to_string(),
                    label: d.label.to_string(),
                    description: d.description.clone(),
                    permission: d.permission,
                    read_only: d.permission.is_read_only(),
                    enabled: tool.enabled() && d.permission.visible_in(mode),
                }
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    /// 查找可调用的工具（同时执行"工具可用"与"模式可见性"检查）
    pub fn find(&self, name: &str, mode: ToolMode) -> Option<Arc<dyn Tool>> {
        let tool = self.by_name.get(name)?;
        if !tool.enabled() || !tool.descriptor().permission.visible_in(mode) {
            return None;
        }
        Some(tool.clone())
    }

    pub fn descriptor_of(&self, name: &str) -> Option<ToolDescriptor> {
        self.by_name.get(name).map(|t| t.descriptor())
    }

    /// 描述该工具是否需要审批
    pub fn permission_of(&self, name: &str) -> Option<Permission> {
        self.by_name.get(name).map(|t| t.descriptor().permission)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use serde_json::{json, Value};

    use super::super::traits::{ToolCtx, ToolOutput};

    struct Dummy {
        name: &'static str,
        permission: Permission,
    }

    #[async_trait::async_trait]
    impl Tool for Dummy {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(
                self.name,
                "占位",
                "测试用工具",
                self.permission,
                json!({"type": "object", "properties": {}}),
            )
        }
        async fn call(&self, _args: Value, _cx: &ToolCtx<'_>) -> Result<ToolOutput> {
            Ok(ToolOutput::text("ok"))
        }
    }

    fn registry() -> ToolRegistry {
        ToolRegistry::new(vec![
            Arc::new(Dummy {
                name: "read_thing",
                permission: Permission::Read,
            }),
            Arc::new(Dummy {
                name: "write_thing",
                permission: Permission::WriteFs,
            }),
            Arc::new(Dummy {
                name: "run_thing",
                permission: Permission::Execute,
            }),
        ])
    }

    #[test]
    fn readonly_mode_hides_write_tools() {
        let reg = registry();
        let names = reg.visible_names(ToolMode::ReadOnly);
        assert_eq!(names, vec!["read_thing"]);
        assert!(reg.find("write_thing", ToolMode::ReadOnly).is_none());
        assert!(reg.find("read_thing", ToolMode::ReadOnly).is_some());
    }

    #[test]
    fn standard_mode_hides_execute_tools() {
        let reg = registry();
        let names = reg.visible_names(ToolMode::Standard);
        assert!(names.contains(&"write_thing".to_string()));
        assert!(!names.contains(&"run_thing".to_string()));
    }

    #[test]
    fn full_mode_shows_everything() {
        let reg = registry();
        assert_eq!(reg.visible_names(ToolMode::Full).len(), 3);
        assert!(reg.find("run_thing", ToolMode::Full).is_some());
    }

    #[test]
    fn schemas_carry_json_schema() {
        let reg = registry();
        let schemas = reg.schemas(ToolMode::Standard);
        let value = serde_json::to_value(&schemas).unwrap();
        assert_eq!(value[0]["type"], "function");
        assert!(value[0]["function"]["parameters"].is_object());
    }

    #[test]
    fn infos_mark_disabled_tools() {
        let reg = registry();
        let infos = reg.infos(ToolMode::ReadOnly);
        let write = infos.iter().find(|i| i.name == "write_thing").unwrap();
        assert!(!write.enabled);
        let read = infos.iter().find(|i| i.name == "read_thing").unwrap();
        assert!(read.enabled);
        assert!(read.read_only);
    }

    #[test]
    fn unknown_tool_is_not_found() {
        let reg = registry();
        assert!(reg.find("does_not_exist", ToolMode::Full).is_none());
    }

    /// 动态来源（MCP）可以在运行中被撤销：`enabled()` 为 false 的工具
    /// 必须立刻从可见集合与可调用集合中消失
    #[test]
    fn disabled_tool_is_invisible_and_uncallable() {
        struct Switchable {
            on: bool,
        }

        #[async_trait::async_trait]
        impl Tool for Switchable {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor::new(
                    "switchable",
                    "可撤销工具",
                    "测试用",
                    Permission::Read,
                    json!({"type": "object", "properties": {}}),
                )
            }
            fn enabled(&self) -> bool {
                self.on
            }
            async fn call(&self, _args: Value, _cx: &ToolCtx<'_>) -> Result<ToolOutput> {
                Ok(ToolOutput::text("ok"))
            }
        }

        let reg = ToolRegistry::new(vec![Arc::new(Switchable { on: false })]);
        assert!(reg.visible_names(ToolMode::Full).is_empty());
        assert!(reg.visible(ToolMode::Full).is_empty());
        assert!(reg.schemas(ToolMode::Full).is_empty());
        assert!(reg.find("switchable", ToolMode::Full).is_none());
        assert!(!reg.infos(ToolMode::Full)[0].enabled);
    }
}
