use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::path::{Component, Path, PathBuf};

use crate::config::types::{
    is_valid_workspace_id, ToolConfig, DEFAULT_WORKSPACE_ID,
};

/// 一个已解析的工作区根目录
#[derive(Debug, Clone)]
pub struct Root {
    pub id: String,
    pub label: String,
    /// 已尽量规范化（存在时是 canonicalize 结果，否则为词法规范化结果）
    pub path: PathBuf,
    pub writable: bool,
    pub is_default: bool,
}

impl Root {
    pub fn available(&self) -> bool {
        self.path.is_dir()
    }
}

/// 前端展示用的工作区状态
#[derive(Debug, Clone, serde::Serialize)]
pub struct RootStatus {
    pub id: String,
    pub label: String,
    pub path: String,
    pub writable: bool,
    pub available: bool,
    pub is_default: bool,
}

/// 解析结果
#[derive(Debug, Clone)]
pub struct Resolved {
    pub root_id: String,
    pub root_path: PathBuf,
    /// 规范化后的绝对路径
    pub abs_path: PathBuf,
    /// 相对于根目录的路径（展示与 deny_glob 用）
    pub rel: PathBuf,
    pub writable: bool,
    pub existed: bool,
}

/// 工作区集合：多根寻址 + 路径监狱 + 拒绝模式
///
/// 所有把字符串变成文件路径的地方都必须经过这里——这是整个工具链唯一
/// 的路径信任边界，工具内部不得自行 `join` 用户/模型提供的路径。
#[derive(Clone)]
pub struct WorkspaceSet {
    roots: Vec<Root>,
    deny: GlobSet,
    deny_patterns: Vec<String>,
    /// 应用自身数据目录（已规范化）
    ///
    /// 默认工作区就在它下面（`%APPDATA%/com.konata-mirror.main/workspace`），
    /// 因此其中的路径必须豁免「AppData 凭据保护」两条内置模式。
    app_data_dir: PathBuf,
    /// 剔除 AppData 两条内置模式后的拒绝集合（仅供应用自身数据目录内使用）
    deny_without_appdata: GlobSet,
}

impl WorkspaceSet {
    pub fn from_config(cfg: &ToolConfig, app_data_dir: &Path) -> Self {
        let mut cfg = cfg.clone();
        cfg.ensure_workspaces();

        let roots = cfg
            .workspaces
            .iter()
            .map(|r| {
                let raw = r.resolve(app_data_dir);
                let path = std::fs::canonicalize(&raw).unwrap_or_else(|_| lexical_normalize(&raw));
                Root {
                    id: r.id.clone(),
                    label: r.label.clone(),
                    path,
                    writable: r.writable,
                    is_default: r.id == DEFAULT_WORKSPACE_ID,
                }
            })
            .collect();

        let deny_patterns = cfg.deny_globs.clone();
        // 应用自身数据目录内的路径不再套用 AppData 两条内置模式，其余条目照旧：
        // 这是默认工作区在 Windows 上可用的前提（详见 APP_DATA_DENY_GLOBS 的注释）
        let without_appdata: Vec<String> = deny_patterns
            .iter()
            .filter(|pattern| {
                !crate::config::types::APP_DATA_DENY_GLOBS.contains(&pattern.as_str())
            })
            .cloned()
            .collect();
        let app_data_dir = std::fs::canonicalize(app_data_dir)
            .unwrap_or_else(|_| lexical_normalize(app_data_dir));

        Self {
            roots,
            deny: build_globset(&deny_patterns),
            deny_patterns,
            app_data_dir,
            deny_without_appdata: build_globset(&without_appdata),
        }
    }

    pub fn list(&self) -> Vec<RootStatus> {
        self.roots
            .iter()
            .map(|r| RootStatus {
                id: r.id.clone(),
                label: r.label.clone(),
                path: r.path.display().to_string(),
                writable: r.writable,
                available: r.available(),
                is_default: r.is_default,
            })
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<&Root> {
        self.roots.iter().find(|r| r.id == id)
    }

    pub fn default_root(&self) -> Option<&Root> {
        self.roots
            .iter()
            .find(|r| r.is_default)
            .or_else(|| self.roots.first())
    }

    pub fn default_path(&self) -> PathBuf {
        self.default_root()
            .map(|r| r.path.clone())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// 供错误信息使用的可用根提示
    pub fn hint(&self) -> String {
        self.roots
            .iter()
            .map(|r| format!("{}（{}）", r.id, r.label))
            .collect::<Vec<_>>()
            .join("、")
    }

    pub fn deny_patterns(&self) -> &[String] {
        &self.deny_patterns
    }

    /// 该绝对路径是否命中敏感文件清单（供搜索类工具过滤结果）
    pub fn is_path_denied(&self, abs: &Path) -> bool {
        match self
            .roots
            .iter()
            .find(|r| abs.starts_with(&r.path))
        {
            Some(root) => self.check_deny(root, abs).is_err(),
            // 不在任何工作区内的路径一律视为不可访问
            None => true,
        }
    }

    /// 解析路径（不要求存在）
    pub fn resolve(&self, raw: &str) -> Result<Resolved, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("路径不能为空".to_string());
        }
        if raw.contains('\0') {
            return Err("路径包含非法字符".to_string());
        }
        // `工作区id:相对路径` 里的冒号是寻址分隔符，不是 NTFS 数据流语法：
        // 平台陷阱检查必须作用在剥离前缀之后的部分
        // 只要前缀长得像工作区 id（且不是 `C:\` 这种盘符），就把冒号当作寻址分隔符；
        // 是否真的存在由 pick_target 判定，届时会给出"未知的工作区 id + 可用列表"
        let hazard_target = raw
            .split_once(':')
            .filter(|(prefix, rest)| {
                is_valid_workspace_id(prefix)
                    && !(prefix.len() == 1
                        && (rest.starts_with('\\') || rest.starts_with('/')))
            })
            .map(|(prefix, _)| &raw[prefix.len() + 1..])
            .unwrap_or(raw);
        check_platform_hazards(hazard_target)?;

        let (root, target) = self.pick_target(raw)?;
        if !root.available() {
            return Err(format!(
                "工作区「{}」当前不可用（目录不存在或不可访问）：{}",
                root.id,
                root.path.display()
            ));
        }

        let candidate = match target {
            Target::Relative(rel) => join_within(&root.path, Path::new(&rel))?,
            Target::Absolute(abs) => abs,
        };

        let (abs_path, existed) = canonicalize_within(&root.path, &candidate)?;
        self.check_deny(root, &abs_path)?;

        let rel = abs_path
            .strip_prefix(&root.path)
            .map(|p| p.to_path_buf())
            .unwrap_or_default();

        Ok(Resolved {
            root_id: root.id.clone(),
            root_path: root.path.clone(),
            abs_path,
            rel,
            writable: root.writable,
            existed,
        })
    }

    /// 解析并要求目标已存在
    pub fn resolve_existing(&self, raw: &str) -> Result<Resolved, String> {
        let resolved = self.resolve(raw)?;
        if !resolved.existed {
            return Err(format!("路径不存在：{}", resolved.abs_path.display()));
        }
        Ok(resolved)
    }

    /// 解析并要求目标是一个已存在的目录
    pub fn resolve_dir(&self, raw: &str) -> Result<Resolved, String> {
        let resolved = self.resolve_existing(raw)?;
        if !resolved.abs_path.is_dir() {
            return Err(format!("不是目录：{}", resolved.abs_path.display()));
        }
        Ok(resolved)
    }

    /// 解析并检查写入权限（根必须是可写的）
    pub fn resolve_writable(&self, raw: &str) -> Result<Resolved, String> {
        let resolved = self.resolve(raw)?;
        if !resolved.writable {
            return Err(format!(
                "工作区「{}」是只读的，如需写入请在设置中开启",
                resolved.root_id
            ));
        }
        Ok(resolved)
    }

    // ─── 内部实现 ───────────────────────────────────────

    fn pick_target<'a>(&'a self, raw: &str) -> Result<(&'a Root, Target), String> {
        // 1) 绝对路径：必须落在某个已注册根内
        if Path::new(raw).is_absolute() {
            if let Some(root) = self.find_root_for_absolute(raw) {
                return Ok((root, Target::Absolute(PathBuf::from(raw))));
            }
            return Err(format!(
                "绝对路径必须位于某个工作区内：{}。可用工作区：{}",
                raw,
                self.hint()
            ));
        }

        // 2) `root_id:相对路径` 寻址
        if let Some((maybe_id, rest)) = raw.split_once(':') {
            // Windows 盘符（单字母 + 反斜杠）不当作工作区 id
            let looks_like_drive = maybe_id.len() == 1
                && (rest.starts_with('\\') || rest.starts_with('/'));
            if !looks_like_drive && is_valid_workspace_id(maybe_id) {
                return match self.get(maybe_id) {
                    Some(root) => {
                        let rel = rest.trim_start_matches(['/', '\\']).to_string();
                        Ok((root, Target::Relative(rel)))
                    }
                    None => Err(format!(
                        "未知的工作区 id「{}」。可用工作区：{}",
                        maybe_id,
                        self.hint()
                    )),
                };
            }
        }

        // 3) 无前缀相对路径 → 默认工作区
        let root = self
            .default_root()
            .ok_or_else(|| "没有任何可用工作区".to_string())?;
        Ok((root, Target::Relative(raw.to_string())))
    }

    fn find_root_for_absolute(&self, raw: &str) -> Option<&Root> {
        let path = Path::new(raw);
        // 已存在的路径用 canonicalize 的结果；不存在的（新建文件）只能按原文比。
        // Windows 上 canonicalize 会带 `\\?\` 前缀，而原文没有，直接 starts_with
        // 必然失败——先统一成可比较形式再判包含关系。
        let candidate = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let candidate = comparable_path(&candidate);
        self.roots
            .iter()
            .find(|r| candidate.starts_with(comparable_path(&r.path)))
    }

    fn check_deny(&self, root: &Root, abs: &Path) -> Result<(), String> {
        let abs_str = to_slash(abs);
        let rel_str = abs.strip_prefix(&root.path).map(to_slash).unwrap_or_default();

        // 默认工作区位于 `%APPDATA%` 下：绝对路径命中 AppData 两条内置模式时，
        // 只要位于应用自身数据目录内就改用剔除后的集合，否则整个默认工作区
        // 都会被拒绝访问。其余内置条目（config.json、*.db、.env…）照常生效。
        let abs_hit = if abs.starts_with(&self.app_data_dir) {
            self.deny_without_appdata.is_match(&abs_str)
        } else {
            self.deny.is_match(&abs_str)
        };
        let hit = abs_hit
            || (!rel_str.is_empty() && self.deny.is_match(&rel_str))
            || self.deny.is_match(
                abs.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
            );

        if hit {
            return Err(format!(
                "该路径命中敏感文件拒绝清单，已阻止访问：{}",
                abs.display()
            ));
        }
        Ok(())
    }
}

/// 路径包含关系比较用的规范化形式
///
/// Windows 的 `canonicalize` 返回 verbatim 路径（`\\?\C:\...`），而模型提供的
/// 不存在的路径只有 `C:\...`；两者必须剥掉前缀后再比。UNC 形式还原成
/// `\\server\share`。Windows 文件系统大小写不敏感，统一小写后比较；
/// 其它平台保持原样（`\` 是合法文件名字符，绝不参与规范化）。
fn comparable_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let stripped: String = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{}", rest)
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else if let Some(rest) = text.strip_prefix("//?/UNC/") {
        format!("//{}", rest)
    } else if let Some(rest) = text.strip_prefix("//?/") {
        rest.to_string()
    } else {
        text.to_string()
    };
    #[cfg(windows)]
    {
        PathBuf::from(stripped.to_ascii_lowercase())
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(stripped)
    }
}

enum Target {
    Relative(String),
    Absolute(PathBuf),
}

fn build_globset(patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        // Windows 下路径大小写不敏感，拒绝模式也必须按不敏感匹配
        if let Ok(glob) = GlobBuilder::new(pattern)
            .case_insensitive(cfg!(windows))
            .build()
        {
            builder.add(glob);
        }
    }
    builder.build().unwrap_or_else(|_| GlobSet::empty())
}

fn to_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// 词法规范化（不访问文件系统，符号链接不会被解析）
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 把相对路径拼到根目录下，任何试图越出的 `..` 都被拒绝
fn join_within(base: &Path, rel: &Path) -> Result<PathBuf, String> {
    let mut out = base.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
                if !out.starts_with(base) {
                    return Err("路径越出工作区（不允许通过 .. 上级目录访问）".to_string());
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("路径越出工作区（不允许使用绝对路径）".to_string());
            }
        }
    }
    Ok(out)
}

/// 规范化并复检包含关系；目标不存在时对最近的已存在祖先做检查
fn canonicalize_within(root: &Path, candidate: &Path) -> Result<(PathBuf, bool), String> {
    if let Ok(canon) = std::fs::canonicalize(candidate) {
        if !canon.starts_with(root) {
            return Err(format!(
                "路径越出工作区（可能是符号链接指向了外部）：{}",
                canon.display()
            ));
        }
        return Ok((canon, true));
    }

    let mut existing = candidate.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while let Some(parent) = existing.parent() {
        if let Some(name) = existing.file_name() {
            tail.push(name.to_os_string());
        }
        existing = parent.to_path_buf();
        if existing.exists() || tail.len() > 64 {
            break;
        }
    }

    let canon_existing = std::fs::canonicalize(&existing)
        .map_err(|e| format!("无法解析路径 {}：{}", existing.display(), e))?;
    if !canon_existing.starts_with(root) {
        return Err(format!(
            "路径越出工作区（可能是符号链接指向了外部）：{}",
            canon_existing.display()
        ));
    }

    let mut out = canon_existing;
    for name in tail.iter().rev() {
        out.push(name);
    }
    if !out.starts_with(root) {
        return Err("路径越出工作区".to_string());
    }
    Ok((out, false))
}

/// 平台相关的路径陷阱（Windows 上尤其多，这里在两端都保持严格）
fn check_platform_hazards(raw: &str) -> Result<(), String> {
    // 扩展长度前缀与 UNC：会绕过常规的路径规范化
    if raw.starts_with(r"\\?\") || raw.starts_with(r"\\.\") {
        return Err("不允许使用 \\\\?\\ / \\\\.\\ 前缀的路径".to_string());
    }
    if raw.starts_with(r"\\") {
        return Err("不允许使用 UNC 网络路径".to_string());
    }

    for comp in Path::new(raw).components() {
        let name = match comp {
            Component::Normal(n) => n.to_string_lossy().to_string(),
            Component::ParentDir => continue,
            _ => continue,
        };
        if name.contains(':') {
            return Err("路径片段中不允许包含冒号（禁用 NTFS 数据流语法）".to_string());
        }
        if name.ends_with('.') || name.ends_with(' ') {
            return Err("路径片段不允许以点或空格结尾".to_string());
        }
        let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
        const DEVICES: &[&str] = &[
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];
        if DEVICES.contains(&stem.as_str()) {
            return Err(format!("不允许使用系统保留设备名：{}", name));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{ToolConfig, WorkspacePath, WorkspaceRoot};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("konata-jail-{}-{}", tag, uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn set_with(root_path: &Path, writable: bool) -> WorkspaceSet {
        let cfg = ToolConfig::with_single_root(root_path, writable, "测试工作区");
        WorkspaceSet::from_config(&cfg, root_path)
    }

    #[test]
    fn relative_path_resolves_inside_default_root() {
        let tmp = TempDir::new("rel");
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "fn main() {}").unwrap();

        let set = set_with(tmp.path(), true);
        let resolved = set.resolve("src/main.rs").unwrap();
        assert_eq!(resolved.root_id, "default");
        assert!(resolved.existed);
        assert!(resolved.abs_path.ends_with("main.rs"));
    }

    #[test]
    fn parent_traversal_is_rejected() {
        let tmp = TempDir::new("escape");
        let set = set_with(tmp.path(), true);
        let err = set.resolve("../../etc/passwd").unwrap_err();
        assert!(err.contains("越出工作区") || err.contains("上级目录"), "{err}");
    }

    #[test]
    fn absolute_path_outside_root_is_rejected() {
        let tmp = TempDir::new("abs");
        let set = set_with(tmp.path(), true);
        let outside = if cfg!(windows) { r"C:\Windows\System32" } else { "/etc/passwd" };
        let err = set.resolve(outside).unwrap_err();
        assert!(err.contains("必须位于某个工作区内"), "{err}");
    }

    #[test]
    fn unknown_root_id_lists_available_roots() {
        let tmp = TempDir::new("id");
        let set = set_with(tmp.path(), true);
        let err = set.resolve("notes:a.md").unwrap_err();
        assert!(err.contains("未知的工作区 id"), "{err}");
        assert!(err.contains("default"), "{err}");
    }

    #[test]
    fn root_id_prefix_is_honoured() {
        let a = TempDir::new("root-a");
        let b = TempDir::new("root-b");

        let cfg = ToolConfig::with_roots(vec![
            WorkspaceRoot {
                id: "default".to_string(),
                label: "A".to_string(),
                path: WorkspacePath::Absolute(a.path().display().to_string()),
                writable: true,
            },
            WorkspaceRoot {
                id: "notes".to_string(),
                label: "B".to_string(),
                path: WorkspacePath::Absolute(b.path().display().to_string()),
                writable: false,
            },
        ]);
        let set = WorkspaceSet::from_config(&cfg, a.path());

        let resolved = set.resolve("notes:todo.md").unwrap();
        assert_eq!(resolved.root_id, "notes");
        assert!(resolved.abs_path.starts_with(std::fs::canonicalize(b.path()).unwrap()));
        assert!(!resolved.writable);

        // 只读根拒绝写入
        assert!(set.resolve_writable("notes:todo.md").is_err());
        // 默认根可写
        assert!(set.resolve_writable("todo.md").is_ok());
    }

    #[test]
    fn deny_globs_block_secrets() {
        let tmp = TempDir::new("deny");
        std::fs::write(tmp.path().join("config.json"), "{\"api_key\":\"sk-x\"}").unwrap();
        std::fs::create_dir_all(tmp.path().join(".ssh")).unwrap();
        std::fs::write(tmp.path().join(".ssh/id_rsa"), "key").unwrap();
        std::fs::write(tmp.path().join("normal.txt"), "ok").unwrap();

        let set = set_with(tmp.path(), true);
        assert!(set.resolve("normal.txt").is_ok());
        assert!(set.resolve("config.json").is_err());
        assert!(set.resolve(".ssh/id_rsa").is_err());
    }

    #[test]
    fn platform_hazards_are_rejected() {
        let tmp = TempDir::new("hazard");
        let set = set_with(tmp.path(), true);

        assert!(set.resolve(r"\\?\C:\Windows").is_err());
        assert!(set.resolve(r"\\server\share\x").is_err());
        assert!(set.resolve("a/b./c").is_err());
        assert!(set.resolve("a/b /c").is_err());
        assert!(set.resolve("CON").is_err());
        assert!(set.resolve("sub/NUL.txt").is_err());
        assert!(set.resolve("file.txt:stream").is_err());
    }

    /// Windows 默认工作区位于 `%APPDATA%` 下：不能因为路径里含
    /// `AppData/Roaming` 就被内置凭据保护规则整体拒绝访问。
    /// 这个用例在 Linux 上用同样的目录名即可复现同一个 glob 匹配。
    #[test]
    fn app_data_globs_do_not_block_the_app_workspace_itself() {
        let tmp = TempDir::new("appdata");
        let app_data = tmp.path().join("AppData").join("Roaming").join("konata");
        let workspace = app_data.join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("note.txt"), "ok").unwrap();
        std::fs::write(workspace.join("config.json"), "{}").unwrap();

        let cfg = ToolConfig::with_single_root(&workspace, true, "默认");
        let set = WorkspaceSet::from_config(&cfg, &app_data);

        assert!(
            set.resolve("note.txt").is_ok(),
            "默认工作区内的普通文件必须可访问：{:?}",
            set.resolve("note.txt").err()
        );
        assert!(set.resolve("src").is_ok());
        assert!(set.resolve(".").is_ok());
        // 豁免的只有 AppData 两条：其它内置拒绝条目照旧生效
        let err = set.resolve("config.json").unwrap_err();
        assert!(err.contains("拒绝清单"), "{err}");
    }

    /// 豁免只针对应用自身数据目录：其它目录哪怕路径里带 AppData 也照旧拒绝
    #[test]
    fn app_data_globs_still_block_paths_outside_app_data() {
        let tmp = TempDir::new("appdata-other");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let workspace = tmp.path().join("AppData").join("Roaming").join("other-app");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("secret.txt"), "x").unwrap();

        let cfg = ToolConfig::with_single_root(&workspace, true, "别的应用");
        let set = WorkspaceSet::from_config(&cfg, &elsewhere);

        let err = set.resolve("secret.txt").unwrap_err();
        assert!(err.contains("拒绝清单"), "{err}");
    }

    /// `\\?\` 前缀必须剥掉后再比较（Windows 上"新建文件的绝对路径"就靠它救）
    #[test]
    fn verbatim_prefix_is_stripped_before_comparison() {
        for (raw, expected) in [
            (r"\\?\C:\ws\new.txt", r"C:\ws\new.txt"),
            (r"\\?\UNC\srv\share\a", r"\\srv\share\a"),
            ("//?/C:/ws/new.txt", "C:/ws/new.txt"),
            ("/tmp/ws/new.txt", "/tmp/ws/new.txt"),
        ] {
            assert_eq!(comparable_path(Path::new(raw)), PathBuf::from(expected), "{raw}");
        }
        // 前缀不同的两侧仍能判定包含关系，且不会把 ws2 误判进 ws
        assert!(comparable_path(Path::new("//?/C:/ws/new.txt"))
            .starts_with(comparable_path(Path::new("//?/C:/ws"))));
        assert!(!comparable_path(Path::new("//?/C:/ws2/a.txt"))
            .starts_with(comparable_path(Path::new("//?/C:/ws"))));
    }

    #[test]
    fn missing_root_is_reported_not_silently_redirected() {
        let tmp = TempDir::new("gone");
        let set = set_with(tmp.path(), true);
        std::fs::remove_dir_all(tmp.path()).unwrap();

        let err = set.resolve("a.txt").unwrap_err();
        assert!(err.contains("不可用"), "{err}");
        assert!(!set.list()[0].available);
    }

    #[test]
    fn default_root_uses_app_data_sentinel() {
        let cfg = ToolConfig::default();
        let app_data = std::path::Path::new("/tmp/konata-appdata");
        assert_eq!(
            cfg.workspaces[0].resolve(app_data),
            app_data.join(crate::config::types::DEFAULT_WORKSPACE_DIR_NAME)
        );
        assert!(cfg.workspaces[0].writable);
    }

    #[test]
    fn nonexistent_target_checks_nearest_existing_ancestor() {
        let tmp = TempDir::new("newfile");
        let set = set_with(tmp.path(), true);
        let resolved = set.resolve("deep/nested/new.txt").unwrap();
        assert!(!resolved.existed);
        assert!(resolved.abs_path.starts_with(std::fs::canonicalize(tmp.path()).unwrap()));
    }
}
