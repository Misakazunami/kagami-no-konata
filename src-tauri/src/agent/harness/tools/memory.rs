use anyhow::Result;
use chrono::Utc;
use serde_json::{json, Value};

use crate::agent::harness::traits::{
    truncate_text, Permission, Tool, ToolCtx, ToolDescriptor, ToolOutput,
};
use crate::llm::proxy::LlmProxy;
use crate::store::memory_store::{MemoryEntry, MemoryType};

use super::args;

/// 检索长期记忆
pub struct SearchMemory;

#[async_trait::async_trait]
impl Tool for SearchMemory {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "search_memory",
            "检索记忆",
            "长期记忆会按需注入上下文；当需要主动回忆用户的偏好、经历或更早的对话细节时使用本工具检索。",
            Permission::Read,
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "检索关键词或自然语言问题" },
                    "top_k": { "type": "integer", "description": "返回条数（默认 5，最大 20）" }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        cx.ensure_not_cancelled()?;
        let query = args::required_str(&args, "query")?;
        let top_k = args::bounded_usize(&args, "top_k", 5, 1, 20);

        let provider = cx
            .services
            .llm_provider
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("当前没有可用的 LLM 提供商，无法做向量检索"))?;
        let store = cx
            .services
            .memory
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("记忆存储不可用"))?;

        // 先 await 嵌入，再取锁：绝不在 await 期间持有 MutexGuard
        let embedding = {
            let proxy = LlmProxy::new(provider);
            let mut vectors = proxy.embed(vec![query.clone()]).await?;
            vectors
                .pop()
                .ok_or_else(|| anyhow::anyhow!("嵌入服务没有返回向量"))?
        };

        let entries = {
            let store = store.lock().unwrap_or_else(|e| e.into_inner());
            store.recall(&embedding, top_k)?
        };

        if entries.is_empty() {
            return Ok(ToolOutput::text(format!("没有检索到与「{}」相关的记忆。", query))
                .with_preview("没有相关记忆"));
        }

        let mut body = format!("与「{}」相关的记忆（{} 条）：\n", query, entries.len());
        for entry in &entries {
            body.push_str(&format!(
                "- [{}] {}（重要度 {:.1}）\n",
                crate::agent::harness::tools::memory::type_label(&entry.memory_type),
                entry.content,
                entry.importance
            ));
        }
        Ok(ToolOutput::text(body).with_preview(format!("命中 {} 条记忆", entries.len())))
    }
}

pub(crate) fn type_label(memory_type: &MemoryType) -> &'static str {
    match memory_type {
        MemoryType::Fact => "事实",
        MemoryType::Preference => "偏好",
        MemoryType::Experience => "经历",
        MemoryType::Emotional => "情感",
    }
}

/// 主动写入一条长期记忆
pub struct SaveMemory;

#[async_trait::async_trait]
impl Tool for SaveMemory {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "save_memory",
            "记住这件事",
            "把一条关于用户的重要信息写入长期记忆（例如偏好、习惯、约定、重要经历）。只在用户明确告知或信息确实值得长期保留时使用。",
            Permission::WriteApp,
            json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "要记住的内容，用第三人称陈述句，例如「用户喜欢喝冰美式」" },
                    "memory_type": {
                        "type": "string",
                        "enum": ["fact", "preference", "experience", "emotional"],
                        "description": "记忆类型，默认 fact"
                    },
                    "importance": { "type": "number", "description": "重要度 0.0~1.0，默认 0.6" }
                },
                "required": ["content"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        cx.ensure_not_cancelled()?;
        let content = args::required_str(&args, "content")?;
        let memory_type =
            MemoryType::from_str(&args::optional_str(&args, "memory_type").unwrap_or_default());
        let importance = args::bounded_f32(&args, "importance", 0.6, 0.0, 1.0);

        let store = cx
            .services
            .memory
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("记忆存储不可用"))?;

        // 已有完全相同内容时不重复写入
        {
            let store = store.lock().unwrap_or_else(|e| e.into_inner());
            let existing = store.list_memories(Some(200)).unwrap_or_default();
            if let Some(hit) = existing.iter().find(|m| m.content.trim() == content.trim()) {
                return Ok(ToolOutput::text(format!("这条记忆已经存在，无需重复记录：{}", hit.content))
                    .with_preview("已存在，未重复写入"));
            }
        }

        // 尽力生成向量；嵌入失败不阻塞写入（后续可由 backfill_embeddings 补齐）
        let embedding = match cx.services.llm_provider.as_ref() {
            Some(provider) => {
                let proxy = LlmProxy::new(provider);
                match proxy.embed(vec![content.clone()]).await {
                    Ok(mut vectors) => vectors.pop(),
                    Err(e) => {
                        eprintln!("[harness] save_memory 嵌入失败，仅保存文本: {}", e);
                        None
                    }
                }
            }
            None => None,
        };

        let now = Utc::now().to_rfc3339();
        let entry = MemoryEntry {
            id: uuid::Uuid::new_v4().to_string(),
            content: content.clone(),
            memory_type,
            importance,
            embedding,
            source_session: cx.session_id.to_string(),
            created_at: now.clone(),
            last_accessed: now,
            access_count: 0,
        };

        {
            let store = store.lock().unwrap_or_else(|e| e.into_inner());
            store.store_memory(&entry)?;
        }

        let text = format!("已记住：{}", content);
        Ok(ToolOutput::text(text.clone()).with_preview(text))
    }
}

/// 列出可用人格
pub struct ListPersonas;

#[async_trait::async_trait]
impl Tool for ListPersonas {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "list_personas",
            "列出人格",
            "列出当前可用的所有角色人格（内置与用户自定义），用于回答「有哪些角色」之类的问题。",
            Permission::Read,
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, _args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let personas = cx
            .services
            .personas
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("人格引擎不可用"))?;
        let engine = personas.read().unwrap_or_else(|e| e.into_inner());
        let list = engine.list_personas();
        if list.is_empty() {
            return Ok(ToolOutput::text("当前没有任何可用人格"));
        }
        let mut body = format!("共有 {} 个人格：\n", list.len());
        for persona in list {
            body.push_str(&format!(
                "- {}（id: {}，版本 {}）\n",
                persona.name, persona.id, persona.version
            ));
        }
        Ok(ToolOutput::text(body))
    }
}

/// 读取某个人格的设定
pub struct ReadPersona;

#[async_trait::async_trait]
impl Tool for ReadPersona {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "read_persona",
            "查看人格设定",
            "读取指定人格的设定内容（性格、说话风格、约束）。用于回答「你是怎么设定的」或对比角色。",
            Permission::Read,
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "人格 id，例如 konata-default；省略则读取当前人格" }
                },
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let personas = cx
            .services
            .personas
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("人格引擎不可用"))?;
        let engine = personas.read().unwrap_or_else(|e| e.into_inner());

        let id = args::optional_str(&args, "id");
        let persona = match &id {
            Some(id) => engine
                .get_persona(id)
                .ok_or_else(|| anyhow::anyhow!("没有找到 id 为「{}」的人格", id))?,
            None => engine
                .default_persona()
                .ok_or_else(|| anyhow::anyhow!("没有可用人格"))?,
        };

        let mut body = format!(
            "人格：{}（id: {}，版本 {}）\n",
            persona.name, persona.id, persona.version
        );
        if !persona.personality.traits.is_empty() {
            body.push_str(&format!("性格：{}\n", persona.personality.traits.join("、")));
        }
        if !persona.personality.speech_style.is_empty() {
            body.push_str(&format!("说话风格：{}\n", persona.personality.speech_style));
        }
        if !persona.constraints.is_empty() {
            body.push_str(&format!("约束：{}\n", persona.constraints.join("；")));
        }
        let (prompt, truncated) = truncate_text(&persona.system_prompt, 4000);
        body.push_str("设定原文：\n");
        body.push_str(&prompt);

        let mut output = ToolOutput::text(body).with_preview(format!("人格：{}", persona.name));
        output.truncated = truncated;
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness::jail::WorkspaceSet;
    use crate::agent::harness::traits::{DenyAllApprover, NullSink, ToolLimits, ToolServices};
    use crate::config::types::{ToolConfig, ToolMode};
    use crate::persona::engine::PersonaEngine;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    fn services_with_personas(tag: &str) -> (std::path::PathBuf, ToolServices) {
        let dir = std::env::temp_dir().join(format!("konata-mem-{}-{}", tag, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = ToolConfig::with_single_root(
            &dir,
            true,
            "测试",
        );
        let set = WorkspaceSet::from_config(&cfg, &dir);
        let mut services = ToolServices::minimal(dir.clone(), set, ToolMode::Standard);
        services.personas = Some(Arc::new(RwLock::new(PersonaEngine::new().unwrap())));
        (dir, services)
    }

    fn ctx<'a>(
        services: &'a ToolServices,
        sink: &'a Arc<NullSink>,
        cancel: &'a Arc<AtomicBool>,
    ) -> ToolCtx<'a> {
        ToolCtx {
            session_id: "s1",
            stream_id: "st1",
            step: 0,
            cancel: cancel.clone(),
            services,
            limits: ToolLimits {
                max_output_bytes: 64 * 1024,
                call_timeout: Duration::from_secs(5),
                approval_timeout: Duration::from_secs(5),
            },
            emit: sink.clone(),
            approver: Arc::new(DenyAllApprover),
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
    fn list_personas_reports_builtin() {
        let (dir, services) = services_with_personas("list");
        let sink = Arc::new(NullSink);
        let cancel = Arc::new(AtomicBool::new(false));
        let cx = ctx(&services, &sink, &cancel);
        let out = block_on(ListPersonas.call(json!({}), &cx)).unwrap();
        assert!(out.content.contains("konata-default"), "{}", out.content);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_persona_defaults_to_current() {
        let (dir, services) = services_with_personas("read");
        let sink = Arc::new(NullSink);
        let cancel = Arc::new(AtomicBool::new(false));
        let cx = ctx(&services, &sink, &cancel);
        let out = block_on(ReadPersona.call(json!({}), &cx)).unwrap();
        assert!(out.content.contains("设定原文"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_persona_unknown_id_errors() {
        let (dir, services) = services_with_personas("unknown");
        let sink = Arc::new(NullSink);
        let cancel = Arc::new(AtomicBool::new(false));
        let cx = ctx(&services, &sink, &cancel);
        assert!(block_on(ReadPersona.call(json!({"id": "不存在"}), &cx)).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_memory_without_store_fails_cleanly() {
        let (dir, services) = services_with_personas("nostore");
        let sink = Arc::new(NullSink);
        let cancel = Arc::new(AtomicBool::new(false));
        let cx = ctx(&services, &sink, &cancel);
        let err = block_on(SaveMemory.call(json!({"content": "用户喜欢猫"}), &cx)).unwrap_err();
        assert!(err.to_string().contains("记忆存储不可用"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn search_memory_requires_provider() {
        let (dir, services) = services_with_personas("noprovider");
        let sink = Arc::new(NullSink);
        let cancel = Arc::new(AtomicBool::new(false));
        let cx = ctx(&services, &sink, &cancel);
        let err = block_on(SearchMemory.call(json!({"query": "猫"}), &cx)).unwrap_err();
        assert!(err.to_string().contains("LLM 提供商"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
