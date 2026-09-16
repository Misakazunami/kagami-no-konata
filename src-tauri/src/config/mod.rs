pub mod types;

use anyhow::Result;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use types::AppConfig;

/// 获取配置文件路径
fn config_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("config.json")
}

/// 获取原子写入使用的临时文件路径（与目标同目录，保证 rename 不跨文件系统）
fn config_tmp_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("config.json.tmp")
}

/// 加载配置
///
/// 容错策略（历史上这里任何错误都会让启动直接 panic，用户只能手改文件）：
/// - 文件不存在 → 写入默认配置
/// - JSON 损坏/缺字段 → 备份损坏文件，回退默认配置并继续启动
/// - providers 为空（历史版本可能写坏）→ 就地重建默认提供商
pub fn load_config(app_data_dir: &Path) -> Result<AppConfig> {
    let path = config_path(app_data_dir);

    if !path.exists() {
        let config = AppConfig::default();
        save_config(app_data_dir, &config)?;
        return Ok(config);
    }

    let content = fs::read_to_string(&path)?;
    let mut config: AppConfig = match serde_json::from_str(&content) {
        Ok(config) => config,
        Err(e) => {
            let backup = backup_corrupt_config(&path);
            eprintln!(
                "[config] 配置文件解析失败，已备份至 {} 并回退默认配置: {}",
                backup.display(),
                e
            );
            let config = AppConfig::default();
            // 回退后的默认配置必须落盘，否则每次启动都会重复走这条容错分支
            save_config(app_data_dir, &config)?;
            return Ok(config);
        }
    };

    // 修复历史坏配置：providers 为空会让整个应用失去 LLM 配置来源
    let mut repaired = config.llm.ensure_non_empty();

    // 活跃 id 悬空（例如指向已被删除的提供商）会让后续保存被 validate 拒绝，
    // 用户会卡在"设置根本存不下去"的状态，这里直接就地修复。
    if !config
        .llm
        .providers
        .iter()
        .any(|p| p.id == config.llm.active_provider_id)
    {
        if let Some(first) = config.llm.providers.first() {
            eprintln!(
                "[config] 活跃提供商 id 悬空，已切换到「{}」",
                first.name
            );
            config.llm.active_provider_id = first.id.clone();
            repaired = true;
        }
    }

    // 内置拒绝清单必须完整：手工编辑/旧构建/另一条开发线的配置可能清空它，
    // 那样 config.json（API Key）、*.db、.env、.ssh/** 会重新对文件工具可读。
    // 就地补齐并落盘；`ToolConfig::validate` 也会拒绝缺失内置条目的配置。
    let missing: Vec<String> = config
        .tools
        .missing_builtin_deny_globs()
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    if !missing.is_empty() {
        eprintln!(
            "[config] 敏感文件拒绝清单缺少 {} 个内置条目，已补齐",
            missing.len()
        );
        config.tools.deny_globs.extend(missing);
        let mut seen = std::collections::HashSet::new();
        config
            .tools
            .deny_globs
            .retain(|pattern| seen.insert(pattern.clone()));
        repaired = true;
    }

    if repaired {
        let _ = save_config(app_data_dir, &config);
    }

    Ok(config)
}

/// 将损坏的配置文件改名保留（便于事后排查/手工恢复）
fn backup_corrupt_config(path: &Path) -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d%H%M%S");
    let backup = path.with_file_name(format!("config.json.corrupt.{}.bak", stamp));
    match fs::rename(path, &backup) {
        Ok(()) => backup,
        Err(e) => {
            eprintln!("[config] 备份损坏配置失败: {}", e);
            path.to_path_buf()
        }
    }
}

/// 保存配置到文件（原子写入）
///
/// 先写同目录临时文件并 `sync_all`，再 `rename` 覆盖目标：
/// 中途崩溃/断电只会留下完整的旧文件或完整的新文件，不会出现半截 JSON
/// ——后者会让下次启动的配置解析失败。
pub fn save_config(app_data_dir: &Path, config: &AppConfig) -> Result<()> {
    fs::create_dir_all(app_data_dir)?;

    let path = config_path(app_data_dir);
    let tmp = config_tmp_path(app_data_dir);
    let content = serde_json::to_string_pretty(config)?;

    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }

    // Windows 上 std::fs::rename 使用 MOVEFILE_REPLACE_EXISTING，可覆盖既有文件
    fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "konata-config-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn load_config_creates_default_when_missing() {
        let dir = temp_dir("create");
        let config = load_config(&dir).expect("load default config");
        assert!(!config.llm.providers.is_empty());
        assert!(config_path(&dir).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_config_recovers_from_corrupt_file() {
        let dir = temp_dir("corrupt");
        fs::write(config_path(&dir), "{ this is not json").unwrap();

        let config = load_config(&dir).expect("must recover instead of failing");
        assert!(!config.llm.providers.is_empty());

        // 坏文件被保留备份，新文件可用
        let backups: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("corrupt"))
            .collect();
        assert_eq!(backups.len(), 1, "损坏的配置必须被备份");
        assert!(serde_json::from_str::<AppConfig>(
            &fs::read_to_string(config_path(&dir)).unwrap()
        )
        .is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_config_repairs_empty_providers() {
        let dir = temp_dir("empty-providers");
        let mut config = AppConfig::default();
        config.llm.providers.clear();
        config.llm.active_provider_id = String::new();
        // 绕开 save_config 直接写入，模拟历史版本留下的坏文件
        fs::write(
            config_path(&dir),
            serde_json::to_string_pretty(&config).unwrap(),
        )
        .unwrap();

        let loaded = load_config(&dir).expect("must repair");
        assert_eq!(loaded.llm.providers.len(), 1);
        assert!(loaded.validate().is_ok());

        // 修复结果已落盘，下次启动不再重复修复
        let reloaded: AppConfig =
            serde_json::from_str(&fs::read_to_string(config_path(&dir)).unwrap()).unwrap();
        assert_eq!(reloaded.llm.providers.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_config_repairs_dangling_active_provider_id() {
        let dir = temp_dir("dangling");
        let mut config = AppConfig::default();
        config.llm.active_provider_id = "ghost".to_string();
        fs::write(
            config_path(&dir),
            serde_json::to_string_pretty(&config).unwrap(),
        )
        .unwrap();

        let loaded = load_config(&dir).expect("must repair dangling id");
        assert_eq!(loaded.llm.active_provider_id, loaded.llm.providers[0].id);
        // 修复后必须能通过校验（否则用户会卡在"设置存不下去"）
        assert!(loaded.validate().is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    /// 内置敏感文件拒绝清单被清空时必须就地补齐并落盘
    ///
    /// 触发场景：手工编辑 config.json、旧构建写出的配置、另一条开发线的配置。
    /// 不修复的话 `read_file("config.json")` 会把 API Key 读进上下文。
    #[test]
    fn load_config_restores_builtin_deny_globs() {
        let dir = temp_dir("deny-restore");
        let mut config = AppConfig::default();
        config.tools.deny_globs.clear();
        fs::write(
            config_path(&dir),
            serde_json::to_string_pretty(&config).unwrap(),
        )
        .unwrap();

        let loaded = load_config(&dir).expect("must repair deny list");
        assert!(
            loaded.tools.missing_builtin_deny_globs().is_empty(),
            "内置拒绝条目必须被补回"
        );
        assert!(loaded.validate().is_ok());

        // 修复结果已落盘，且重复加载不会重复追加
        let reloaded: AppConfig =
            serde_json::from_str(&fs::read_to_string(config_path(&dir)).unwrap()).unwrap();
        assert!(reloaded.tools.missing_builtin_deny_globs().is_empty());
        let again = load_config(&dir).expect("second load");
        assert_eq!(again.tools.deny_globs.len(), reloaded.tools.deny_globs.len());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_config_is_atomic_and_leaves_no_tmp_file() {
        let dir = temp_dir("atomic");
        save_config(&dir, &AppConfig::default()).unwrap();
        assert!(config_path(&dir).exists());
        assert!(!config_tmp_path(&dir).exists(), "临时文件必须已被 rename");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_with_missing_sections_still_loads() {
        let dir = temp_dir("partial");
        // 只有 llm 段的旧配置：user / ui / memory 应回退到默认值
        let mut config = AppConfig::default();
        config.llm.providers[0].api_key = "sk-kept".to_string();
        let full = serde_json::to_value(&config).unwrap();
        let mut partial = serde_json::Map::new();
        partial.insert("llm".to_string(), full.get("llm").unwrap().clone());
        fs::write(
            config_path(&dir),
            serde_json::to_string(&serde_json::Value::Object(partial)).unwrap(),
        )
        .unwrap();

        let loaded = load_config(&dir).expect("partial config must load");
        assert_eq!(loaded.llm.providers[0].api_key, "sk-kept");
        assert_eq!(loaded.ui.theme, "dark");
        assert_eq!(loaded.memory.max_context_memories, 5);
        let _ = fs::remove_dir_all(&dir);
    }
}
