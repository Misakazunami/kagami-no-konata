use anyhow::Result;
use globset::Glob;
use regex::Regex;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::agent::harness::traits::{
    truncate_text, Permission, Tool, ToolCtx, ToolDescriptor, ToolOutput,
};

use super::args;

/// 单次搜索返回的结果条数上限
const MAX_SEARCH_RESULTS: usize = 200;
/// 单个文件参与 grep 的体积上限（超过视为不可搜索的大文件）
const MAX_GREP_FILE_BYTES: u64 = 1024 * 1024;
/// 可整体读入内存的文本文件上限（read_file / edit_file 共用）
///
/// 超过这个大小就明确拒绝：GB 级日志/镜像一次 `fs::read` 会申请同样大小的
/// 内存（还有解码副本），足以把应用 OOM 掉。
pub(crate) const MAX_TEXT_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// 二进制探测窗口
const BINARY_SNIFF_BYTES: usize = 8192;

/// 读取文本文件
pub struct ReadFile;

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "read_file",
            "读取文件",
            "读取工作区内文本文件的内容，带行号，可用 offset/limit 分页。路径可以是相对默认工作区的相对路径，或 `工作区id:相对路径`。",
            Permission::Read,
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径。相对路径基于默认工作区；也可写成 `notes:docs/a.md` 指定工作区"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "起始行号（从 1 开始，默认 1）"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "最多读取的行数（默认 400，最大 2000）"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        cx.ensure_not_cancelled()?;
        let raw_path = args::required_str(&args, "path")?;
        let offset = args::bounded_usize(&args, "offset", 1, 1, 1_000_000);
        let limit = args::bounded_usize(&args, "limit", 400, 1, 2000);

        let resolved = cx.services.workspaces.resolve_existing(&raw_path).map_err(anyhow::Error::msg)?;
        if resolved.abs_path.is_dir() {
            anyhow::bail!(
                "{} 是目录，请改用 list_dir",
                resolved.abs_path.display()
            );
        }

        let metadata = std::fs::metadata(&resolved.abs_path)?;
        if metadata.len() > MAX_TEXT_FILE_BYTES {
            anyhow::bail!(
                "文件过大（{} MB，上限 {} MB）：请用 grep_search 定位内容，或用 offset/limit 之外的工具分片查看",
                metadata.len() / 1024 / 1024,
                MAX_TEXT_FILE_BYTES / 1024 / 1024
            );
        }
        let bytes = std::fs::read(&resolved.abs_path)?;
        if is_binary(&bytes) {
            anyhow::bail!(
                "{} 是二进制文件（{} 字节），无法作为文本读取",
                resolved.abs_path.display(),
                bytes.len()
            );
        }
        let text = String::from_utf8_lossy(&bytes).to_string();

        let all_lines: Vec<&str> = text.lines().collect();
        let total = all_lines.len();
        if offset > total && total > 0 {
            anyhow::bail!("起始行 {} 超出文件总行数 {}", offset, total);
        }
        let start = (offset - 1).min(total);
        let end = (start + limit).min(total);

        let mut body = String::new();
        for (index, line) in all_lines[start..end].iter().enumerate() {
            body.push_str(&format!("{:>6}│{}\n", start + index + 1, line));
        }

        let header = format!(
            "文件：{}（工作区 {}，共 {} 行，本次显示 {}-{} 行）\n",
            resolved.abs_path.display(),
            resolved.root_id,
            total,
            if total == 0 { 0 } else { start + 1 },
            end
        );
        let (body, truncated) = truncate_text(&body, cx.limits.max_output_bytes);
        let mut output = ToolOutput::text(format!("{}{}", header, body));
        output.truncated = truncated;
        Ok(output.with_preview(header))
    }
}

/// 列出目录内容
pub struct ListDir;

#[async_trait::async_trait]
impl Tool for ListDir {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "list_dir",
            "列出目录",
            "列出工作区内某个目录的条目（子目录在前，带文件大小）。路径省略时列出默认工作区根目录。",
            Permission::Read,
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "目录路径；省略表示默认工作区根目录"
                    }
                },
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        cx.ensure_not_cancelled()?;
        let raw = args::optional_str(&args, "path").unwrap_or_default();
        let resolved = if raw.is_empty() {
            cx.services.workspaces.resolve_dir(".").map_err(anyhow::Error::msg)?
        } else {
            cx.services.workspaces.resolve_dir(&raw).map_err(anyhow::Error::msg)?
        };

        let mut dirs: Vec<String> = Vec::new();
        let mut files: Vec<String> = Vec::new();
        let mut skipped = 0usize;

        for entry in std::fs::read_dir(&resolved.abs_path)? {
            // 单个条目在枚举与 stat 之间被并发删除/权限异常 → 跳过该条目，
            // 而不是让整次 list_dir 失败
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if cx.services.workspaces.is_path_denied(&path) {
                skipped += 1;
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(metadata) = entry.metadata() else { continue };
            if metadata.is_dir() {
                dirs.push(format!("{}/", name));
            } else {
                files.push(format!("{}  ({} 字节)", name, metadata.len()));
            }
        }
        dirs.sort();
        files.sort();

        let mut body = format!(
            "目录：{}（工作区 {}）\n",
            resolved.abs_path.display(),
            resolved.root_id
        );
        if dirs.is_empty() && files.is_empty() {
            body.push_str("（空目录）\n");
        }
        for dir in &dirs {
            body.push_str(dir);
            body.push('\n');
        }
        for file in &files {
            body.push_str(file);
            body.push('\n');
        }
        if skipped > 0 {
            body.push_str(&format!("（已隐藏 {} 个敏感文件）\n", skipped));
        }

        let (body, truncated) = truncate_text(&body, cx.limits.max_output_bytes);
        let mut output = ToolOutput::text(body);
        output.truncated = truncated;
        Ok(output)
    }
}

/// 按文件名模式搜索
pub struct GlobSearch;

#[async_trait::async_trait]
impl Tool for GlobSearch {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "glob_search",
            "按文件名搜索",
            "在工作区内按 glob 模式查找文件路径，例如 `**/*.rs`、`src/**/*.ts`。只返回路径，不返回内容。",
            Permission::Read,
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "glob 模式，如 **/*.md" },
                    "root": { "type": "string", "description": "限定搜索的工作区 id；省略表示默认工作区" },
                    "max_results": { "type": "integer", "description": "最多返回条数（默认 100，最大 200）" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        cx.ensure_not_cancelled()?;
        let pattern = args::required_str(&args, "pattern")?;
        let max = args::bounded_usize(&args, "max_results", 100, 1, MAX_SEARCH_RESULTS);

        let glob = Glob::new(&pattern)
            .map_err(|e| anyhow::anyhow!("glob 模式不合法：{}", e))?
            .compile_matcher();

        let root_arg = args::optional_str(&args, "root");
        let search_root = match &root_arg {
            Some(id) => cx.services.workspaces.resolve_dir(&format!("{}:.", id)).map_err(anyhow::Error::msg)?,
            None => cx.services.workspaces.resolve_dir(".").map_err(anyhow::Error::msg)?,
        };

        let mut hits: Vec<String> = Vec::new();
        let mut scanned = 0usize;
        for entry in walkdir::WalkDir::new(&search_root.abs_path)
            .max_depth(24)
            .into_iter()
            // 谓词同样作用于根条目：根名命中隐藏列表时整棵树会被剪掉，
            // 因此根（depth 0）必须放行
            .filter_entry(|e| e.depth() == 0 || !is_hidden_dir(e.path()))
        {
            if cx.cancelled() {
                cx.ensure_not_cancelled()?;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            scanned += 1;
            let path = entry.path();
            if cx.services.workspaces.is_path_denied(path) {
                continue;
            }
            let relative = path
                .strip_prefix(&search_root.abs_path)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            if relative.is_empty() {
                continue;
            }
            if glob.is_match(&relative) || glob.is_match(format!("/{}", relative)) {
                let marker = if entry.file_type().is_dir() { "/" } else { "" };
                hits.push(format!(
                    "{}{}  [{}]",
                    relative, marker, search_root.root_id
                ));
                if hits.len() >= max {
                    break;
                }
            }
        }
        hits.sort();

        let header = format!(
            "模式 `{}` 在工作区 {} 中共命中 {} 条（已扫描 {} 个条目{})",
            pattern,
            search_root.root_id,
            hits.len(),
            scanned,
            if hits.len() >= max { "，已达上限" } else { "" }
        );
        let mut body = header.clone();
        body.push('\n');
        body.push_str(&hits.join("\n"));
        let (body, truncated) = truncate_text(&body, cx.limits.max_output_bytes);
        let mut output = ToolOutput::text(body);
        output.truncated = truncated;
        Ok(output.with_preview(header))
    }
}

/// 按内容搜索（正则）
pub struct GrepSearch;

#[async_trait::async_trait]
impl Tool for GrepSearch {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "grep_search",
            "按内容搜索",
            "在工作区文件中用正则搜索内容，返回 `文件:行号: 内容`。适合定位代码、配置或文档中的关键字。",
            Permission::Read,
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Rust 正则表达式" },
                    "path": { "type": "string", "description": "限定搜索的子目录或文件；省略表示默认工作区" },
                    "file_glob": { "type": "string", "description": "只搜索匹配该 glob 的文件，如 **/*.rs" },
                    "max_results": { "type": "integer", "description": "最多返回条数（默认 60，最大 200）" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        cx.ensure_not_cancelled()?;
        let pattern = args::required_str(&args, "pattern")?;
        let max = args::bounded_usize(&args, "max_results", 60, 1, MAX_SEARCH_RESULTS);
        let regex = Regex::new(&pattern).map_err(|e| anyhow::anyhow!("正则表达式不合法：{}", e))?;

        let file_filter = args::optional_str(&args, "file_glob")
            .map(|g| Glob::new(&g).map(|glob| glob.compile_matcher()))
            .transpose()
            .map_err(|e| anyhow::anyhow!("file_glob 不合法：{}", e))?;

        let target = match args::optional_str(&args, "path") {
            Some(path) => cx.services.workspaces.resolve_existing(&path).map_err(anyhow::Error::msg)?,
            None => cx.services.workspaces.resolve_dir(".").map_err(anyhow::Error::msg)?,
        };

        let mut hits: Vec<String> = Vec::new();
        let mut files_scanned = 0usize;

        // 惰性遍历：历史实现先把全部路径 collect 进 Vec，数十万文件的工作区
        // 会先吃掉几百 MB 内存，而且收集阶段无法响应取消
        let files: Box<dyn Iterator<Item = PathBuf>> = if target.abs_path.is_file() {
            Box::new(std::iter::once(target.abs_path.clone()))
        } else {
            Box::new(
                walkdir::WalkDir::new(&target.abs_path)
                    .max_depth(24)
                    .into_iter()
                    // 根条目也要放行（见 glob_search 的同款说明）
                    .filter_entry(|e| e.depth() == 0 || !is_hidden_dir(e.path()))
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file())
                    .map(|e| e.into_path()),
            )
        };

        'outer: for file in files {
            if cx.cancelled() {
                cx.ensure_not_cancelled()?;
            }
            if cx.services.workspaces.is_path_denied(&file) {
                continue;
            }
            if let Some(filter) = &file_filter {
                let relative = file
                    .strip_prefix(&target.abs_path)
                    .unwrap_or(&file)
                    .to_string_lossy()
                    .replace('\\', "/");
                if !filter.is_match(&relative) && !filter.is_match(file.to_string_lossy().as_ref())
                {
                    continue;
                }
            }
            let metadata = match std::fs::metadata(&file) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.len() > MAX_GREP_FILE_BYTES {
                continue;
            }
            let bytes = match std::fs::read(&file) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if is_binary(&bytes) {
                continue;
            }
            files_scanned += 1;
            let text = String::from_utf8_lossy(&bytes);
            let display_path = file
                .strip_prefix(&target.abs_path)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| file.to_string_lossy().to_string());

            for (number, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    let trimmed = line.trim();
                    let preview = truncate_text(trimmed, 300).0;
                    hits.push(format!("{}:{}: {}", display_path, number + 1, preview));
                    if hits.len() >= max {
                        break 'outer;
                    }
                }
            }
        }

        let header = format!(
            "正则 `{}` 命中 {} 条（已扫描 {} 个文本文件{}）",
            pattern,
            hits.len(),
            files_scanned,
            if hits.len() >= max { "，已达上限" } else { "" }
        );
        let mut body = header.clone();
        if hits.is_empty() {
            body.push_str("\n（没有匹配内容）");
        } else {
            body.push('\n');
            body.push_str(&hits.join("\n"));
        }
        let (body, truncated) = truncate_text(&body, cx.limits.max_output_bytes);
        let mut output = ToolOutput::text(body);
        output.truncated = truncated;
        Ok(output.with_preview(header))
    }
}

/// 二进制探测：前 8 KB 出现 NUL 即视为二进制
pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .take(BINARY_SNIFF_BYTES)
        .any(|byte| *byte == 0)
}

/// 跳过隐藏目录（.git / node_modules / target 等），避免搜索退化
fn is_hidden_dir(path: &Path) -> bool {
    let name = match path.file_name() {
        Some(name) => name.to_string_lossy().to_string(),
        None => return false,
    };
    matches!(
        name.as_str(),
        ".git" | "node_modules" | "target" | "dist" | ".svn" | ".hg" | "__pycache__" | ".venv"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness::jail::WorkspaceSet;
    use crate::agent::harness::traits::{
        DenyAllApprover, NullSink, ToolLimits, ToolServices,
    };
    use crate::config::types::{ToolConfig, ToolMode};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;

    struct Fixture {
        dir: PathBuf,
        services: ToolServices,
        sink: Arc<NullSink>,
        cancel: Arc<AtomicBool>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("konata-fs-{}-{}", tag, uuid::Uuid::new_v4()));
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(dir.join("README.md"), "# 标题\n第二行\n第三行\n").unwrap();
            std::fs::write(dir.join("src/main.rs"), "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
            std::fs::write(dir.join("secret.txt"), "ok").unwrap();
            std::fs::write(dir.join("config.json"), "{\"api_key\":\"sk-x\"}").unwrap();

            let cfg = ToolConfig::with_single_root(
                &dir,
                true,
                "测试",
            );
            let set = WorkspaceSet::from_config(&cfg, &dir);
            Self {
                services: ToolServices::minimal(dir.clone(), set, ToolMode::Standard),
                dir,
                sink: Arc::new(NullSink),
                cancel: Arc::new(AtomicBool::new(false)),
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
    fn read_file_returns_numbered_lines() {
        let fx = Fixture::new("read");
        let cx = fx.ctx();
        let out = block_on(ReadFile.call(json!({"path": "README.md"}), &cx)).unwrap();
        assert!(out.content.contains("共 3 行"));
        assert!(out.content.contains("1│# 标题"));
    }

    #[test]
    fn read_file_paginates() {
        let fx = Fixture::new("page");
        let cx = fx.ctx();
        let out = block_on(ReadFile.call(
            json!({"path": "README.md", "offset": 2, "limit": 1}),
            &cx,
        ))
        .unwrap();
        assert!(out.content.contains("本次显示 2-2 行"));
        assert!(out.content.contains("第二行"));
        assert!(!out.content.contains("第三行"));
    }

    #[test]
    fn read_file_refuses_sensitive_and_directory() {
        let fx = Fixture::new("deny");
        let cx = fx.ctx();
        assert!(block_on(ReadFile.call(json!({"path": "config.json"}), &cx)).is_err());
        let err = block_on(ReadFile.call(json!({"path": "src"}), &cx)).unwrap_err();
        assert!(err.to_string().contains("是目录"));
    }

    #[test]
    fn list_dir_hides_sensitive_files() {
        let fx = Fixture::new("list");
        let cx = fx.ctx();
        let out = block_on(ListDir.call(json!({}), &cx)).unwrap();
        assert!(out.content.contains("src/"));
        assert!(out.content.contains("README.md"));
        assert!(out.content.contains("secret.txt"));
        assert!(!out.content.contains("config.json"), "敏感文件必须隐藏");
        assert!(out.content.contains("已隐藏"));
    }

    #[test]
    fn glob_search_finds_files_by_pattern() {
        let fx = Fixture::new("glob");
        let cx = fx.ctx();
        let out = block_on(GlobSearch.call(json!({"pattern": "**/*.rs"}), &cx)).unwrap();
        assert!(out.content.contains("src/main.rs"));
        assert!(!out.content.contains("README.md"));
    }

    #[test]
    fn glob_search_rejects_bad_pattern() {
        let fx = Fixture::new("badglob");
        let cx = fx.ctx();
        assert!(block_on(GlobSearch.call(json!({"pattern": "[[["}), &cx)).is_err());
    }

    #[test]
    fn grep_search_returns_file_line_matches() {
        let fx = Fixture::new("grep");
        let cx = fx.ctx();
        let out = block_on(GrepSearch.call(json!({"pattern": "println"}), &cx)).unwrap();
        assert!(out.content.contains("src/main.rs:2:"));
        assert!(out.content.contains("println"));
    }

    #[test]
    fn grep_search_skips_sensitive_files() {
        let fx = Fixture::new("grepdeny");
        let cx = fx.ctx();
        let out = block_on(GrepSearch.call(json!({"pattern": "api_key"}), &cx)).unwrap();
        assert!(out.content.contains("命中 0 条"), "{}", out.content);
    }

    #[test]
    fn grep_search_rejects_invalid_regex() {
        let fx = Fixture::new("badregex");
        let cx = fx.ctx();
        assert!(block_on(GrepSearch.call(json!({"pattern": "("}), &cx)).is_err());
    }

    #[test]
    fn binary_files_are_detected() {
        assert!(is_binary(b"\x00\x01\x02"));
        assert!(!is_binary("普通文本".as_bytes()));
    }
}
