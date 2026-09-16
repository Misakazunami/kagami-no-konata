//! 回收站（Linux/macOS）与"不可用时的显式拒绝"
//!
//! 为什么自己写而不引第三方 crate：这一层必须**可测**且行为明确。
//! 引入 `trash` crate 会同时带进各平台的 COM/Objective-C 绑定，
//! 而本项目只在 Linux 上验证，Windows 行为将完全未经测试。
//! 因此这里采取"能安全回收就回收，做不到就明确拒绝"的策略：
//!
//! - Linux：按 XDG Trash 规范写入 `$XDG_DATA_HOME/Trash/{files,info}`；
//! - macOS：`~/.Trash`；
//! - Windows：**不支持**（返回错误），除非调用方显式要求"永久删除"。
//!   宁可让模型/用户看到"这里不能删"，也不要静默永久删除用户文件。
//!
//! 关键不变式：**先确保文件已经被安全安置，再让它从原位置消失**。
//! 跨卷（rename 返回 EXDEV）时退化为"复制到回收站 + 删除原文件"，
//! 复制失败就整体放弃（原文件不动）。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

/// 文件最终去了哪里
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// 已移入回收站（用户可从系统回收站找回）
    Trashed,
    /// 已永久删除
    Permanent,
}

impl Removal {
    pub fn label(self) -> &'static str {
        match self {
            Removal::Trashed => "已移入回收站",
            Removal::Permanent => "已永久删除",
        }
    }
}

/// 本平台是否支持回收站
pub fn is_supported() -> bool {
    if cfg!(target_os = "linux") {
        return trash_root().is_ok();
    }
    if cfg!(target_os = "macos") {
        return home_dir().map(|h| h.join(".Trash")).is_some();
    }
    false
}

/// 不支持时给出的人类可读原因（写进工具错误信息，让模型知道该怎么办）
pub fn unsupported_reason() -> String {
    format!(
        "当前平台（{}）没有可用的回收站，为避免误删无法恢复：如需真删除请显式设置 permanent=true（会在审批弹窗里标红提示）",
        std::env::consts::OS
    )
}

/// 把文件/目录移入回收站
pub fn move_to_trash(target: &Path) -> Result<PathBuf> {
    if !is_supported() {
        anyhow::bail!("{}", unsupported_reason());
    }
    let (files_dir, info_dir) = trash_dirs()?;
    move_to_trash_in(target, &files_dir, &info_dir)
}

/// 显式指定回收站目录的实现
///
/// 拆出这一层是为了可测：生产环境按 XDG 解析目录，测试注入临时目录，
/// 否则测试要么污染用户真实回收站、要么去改 `XDG_DATA_HOME` 这个全局变量
/// （并行跑测试时会互相打架）。
fn move_to_trash_in(target: &Path, files_dir: &Path, info_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(files_dir).with_context(|| format!("创建回收站目录失败：{}", files_dir.display()))?;
    fs::create_dir_all(info_dir).with_context(|| format!("创建回收站信息目录失败：{}", info_dir.display()))?;

    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "item".to_string());

    // 同名的回收站条目已存在时改名，保证 files 与 info 同名配对
    let (_entry_name, trashed_path, info_path) = pick_free_name(files_dir, info_dir, &name)?;

    // 先写 info：万一写入失败，文件还没被动过，原位置完好
    let info_body = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        encode_path(&absolute(target)?),
        format_deletion_date()
    );
    fs::write(&info_path, info_body)
        .with_context(|| format!("写入回收站信息失败：{}", info_path.display()))?;

    match fs::rename(target, &trashed_path) {
        Ok(()) => Ok(trashed_path),
        Err(rename_err) => {
            // 跨卷（EXDEV）等情形：改为"复制成功后再删原文件"
            if let Err(copy_err) = copy_into_place(target, &trashed_path) {
                // 复制失败 → 清理刚写的 info，原文件保持不动（fail-closed）
                let _ = fs::remove_file(&info_path);
                anyhow::bail!(
                    "无法移入回收站（{}），且复制失败（{}）：为避免丢失，未删除 {}",
                    rename_err,
                    copy_err,
                    target.display()
                );
            }
            if let Err(remove_err) = remove_any(target) {
                // 复制成功但原文件删不掉：把回收站里的副本撤掉，避免出现"两份"
                let _ = remove_any(&trashed_path);
                let _ = fs::remove_file(&info_path);
                anyhow::bail!(
                    "已复制到回收站但无法删除原文件（{}），已回退：{}",
                    remove_err,
                    target.display()
                );
            }
            Ok(trashed_path)
        }
    }
}

/// 永久删除（调用方必须已经在审批里明确告知用户）
pub fn remove_permanently(target: &Path) -> Result<()> {
    remove_any(target)
        .with_context(|| format!("永久删除失败：{}", target.display()))
}

fn remove_any(target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(target)
        .with_context(|| format!("无法读取 {}", target.display()))?;
    if metadata.is_dir() {
        fs::remove_dir_all(target).map_err(|e| anyhow::anyhow!(e))
    } else {
        fs::remove_file(target).map_err(|e| anyhow::anyhow!(e))
    }
}

/// 复制到回收站目标位置（文件用复制，目录递归复制）
fn copy_into_place(target: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(target)?;
    if metadata.is_dir() {
        copy_tree(target, destination)
    } else {
        fs::copy(target, destination).map(|_| ()).map_err(|e| anyhow::anyhow!(e))
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let child_from = entry.path();
        let child_to = to.join(entry.file_name());
        let metadata = fs::symlink_metadata(&child_from)?;
        if metadata.is_dir() {
            copy_tree(&child_from, &child_to)?;
        } else {
            fs::copy(&child_from, &child_to)?;
        }
    }
    Ok(())
}

/// 找一个还没被占用的条目名（`name`、`name.1`、`name.2`…）
fn pick_free_name(
    files_dir: &Path,
    info_dir: &Path,
    name: &str,
) -> Result<(String, PathBuf, PathBuf)> {
    for attempt in 0..1000 {
        let candidate = if attempt == 0 {
            name.to_string()
        } else {
            format!("{}.{}", name, attempt)
        };
        let files_path = files_dir.join(&candidate);
        let info_path = info_dir.join(format!("{}.trashinfo", candidate));
        if !files_path.exists() && !info_path.exists() {
            return Ok((candidate, files_path, info_path));
        }
    }
    anyhow::bail!("回收站里同名条目过多，无法为「{}」生成唯一名字", name)
}

fn trash_dirs() -> Result<(PathBuf, PathBuf)> {
    let root = trash_root()?;
    Ok((root.join("files"), root.join("info")))
}

fn trash_root() -> Result<PathBuf> {
    if cfg!(target_os = "linux") {
        // XDG 规范：$XDG_DATA_HOME/Trash，缺省 ~/.local/share/Trash
        if let Some(data_home) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(data_home).join("Trash"));
        }
        let home = home_dir().ok_or_else(|| anyhow::anyhow!("无法确定用户主目录（HOME 未设置）"))?;
        return Ok(home.join(".local").join("share").join("Trash"));
    }
    if cfg!(target_os = "macos") {
        let home = home_dir().ok_or_else(|| anyhow::anyhow!("无法确定用户主目录（HOME 未设置）"))?;
        return Ok(home.join(".Trash"));
    }
    anyhow::bail!("{}", unsupported_reason())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn absolute(target: &Path) -> Result<String> {
    let abs = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir()?.join(target)
    };
    Ok(abs.to_string_lossy().to_string())
}

/// RFC2396 风格的百分号编码（XDG Trash Info 的 `Path` 键要求 URL 编码）
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 8);
    for byte in path.bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~');
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

/// `YYYY-MM-DDTHH:MM:SS`（回收站规范要求本地时间、秒级、无时区）
fn format_deletion_date() -> String {
    let now = SystemTime::now();
    let datetime: chrono::DateTime<chrono::Local> = now.into();
    datetime.format("%Y-%m-%dT%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        files_dir: PathBuf,
        info_dir: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("konata-trash-{}-{}", tag, uuid::Uuid::new_v4()));
            let files_dir = root.join("Trash/files");
            let info_dir = root.join("Trash/info");
            std::fs::create_dir_all(&root).unwrap();
            Self {
                root,
                files_dir,
                info_dir,
            }
        }

        /// 走"注入目录"的入口：不碰真实的回收站，也不动 XDG_DATA_HOME
        fn trash(&self, target: &Path) -> Result<PathBuf> {
            move_to_trash_in(target, &self.files_dir, &self.info_dir)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn moves_file_into_trash_with_info() {
        let fx = Fixture::new("file");
        let victim = fx.root.join("note.txt");
        std::fs::write(&victim, "内容").unwrap();

        let trashed = fx.trash(&victim).expect("应当成功移入回收站");
        assert!(!victim.exists(), "原位置必须已经不存在");
        assert!(trashed.exists(), "回收站里必须有这个文件");
        assert_eq!(std::fs::read_to_string(&trashed).unwrap(), "内容");

        let info = fx.info_dir.join(format!(
            "{}.trashinfo",
            trashed.file_name().unwrap().to_string_lossy()
        ));
        let body = std::fs::read_to_string(&info).expect("必须写 trashinfo");
        assert!(body.starts_with("[Trash Info]"), "{body}");
        assert!(body.contains("Path="), "{body}");
        assert!(body.contains("DeletionDate="), "{body}");
    }

    #[test]
    fn name_collisions_do_not_overwrite() {
        let fx = Fixture::new("collision");
        let first = fx.root.join("dup.txt");
        std::fs::write(&first, "第一次").unwrap();
        let trashed_first = fx.trash(&first).unwrap();

        let second = fx.root.join("dup.txt");
        std::fs::write(&second, "第二次").unwrap();
        let trashed_second = fx.trash(&second).unwrap();

        assert_ne!(
            trashed_first, trashed_second,
            "同名文件不能覆盖回收站里的旧条目"
        );
        assert_eq!(std::fs::read_to_string(&trashed_first).unwrap(), "第一次");
        assert_eq!(std::fs::read_to_string(&trashed_second).unwrap(), "第二次");
    }

    #[test]
    fn moves_directory_tree() {
        let fx = Fixture::new("dir");
        let dir = fx.root.join("nested");
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        std::fs::write(dir.join("inner/deep.txt"), "深层文件").unwrap();

        let trashed = fx.trash(&dir).unwrap();
        assert!(!dir.exists());
        assert!(
            trashed.join("inner/deep.txt").exists(),
            "目录树必须整体过去"
        );
    }

    #[test]
    fn missing_target_is_an_error_and_touches_nothing() {
        let fx = Fixture::new("missing");
        let ghost = fx.root.join("nope.txt");
        assert!(fx.trash(&ghost).is_err());
        // 失败时不应该留下垃圾 info 文件
        let leftovers = std::fs::read_dir(&fx.info_dir)
            .map(|dir| dir.count())
            .unwrap_or(0);
        assert_eq!(leftovers, 0, "失败路径必须清理掉刚写的 trashinfo");
    }

    #[test]
    fn path_is_percent_encoded() {
        assert_eq!(encode_path("/home/me/a b.txt"), "/home/me/a%20b.txt");
        assert_eq!(encode_path("/tmp/中文.txt"), "/tmp/%E4%B8%AD%E6%96%87.txt");
        // 安全字符保持原样，便于人读
        assert_eq!(encode_path("/a-b_c.d~e/f"), "/a-b_c.d~e/f");
    }

    #[test]
    fn deletion_date_has_trash_format() {
        let stamp = format_deletion_date();
        let re = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}$").unwrap();
        assert!(re.is_match(&stamp), "{stamp}");
    }

    #[test]
    fn permanent_removal_deletes_directory_tree() {
        let fx = Fixture::new("perm");
        let dir = fx.root.join("gone");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/f.txt"), "x").unwrap();
        remove_permanently(&dir).unwrap();
        assert!(!dir.exists());
    }
}
