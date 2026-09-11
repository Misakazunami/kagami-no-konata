use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use super::types::PersonaConfig;

/// 预置默认人格 YAML（嵌入二进制）
const DEFAULT_PERSONA_YAML: &str = include_str!("../../personas/default.yaml");

/// 人格引擎
pub struct PersonaEngine {
    personas: HashMap<String, PersonaConfig>,
    /// 内置默认人格 ID（用于确定性回退，而非随机的 HashMap 首个元素）
    default_id: String,
}

impl PersonaEngine {
    /// 初始化，加载预置人格
    pub fn new() -> Result<Self> {
        let mut personas = HashMap::new();

        // 加载内置默认人格
        let default: PersonaConfig = serde_yaml::from_str(DEFAULT_PERSONA_YAML)?;
        let default_id = default.id.clone();
        personas.insert(default.id.clone(), default);

        Ok(Self { personas, default_id })
    }

    /// 初始化并加载全部人格（内置 + 磁盘用户自定义）
    pub fn load_all(app_data_dir: &Path) -> Result<Self> {
        let mut engine = Self::new()?;
        engine.load_from_disk(app_data_dir);
        Ok(engine)
    }

    /// 从磁盘加载用户自定义人格（合并到已有列表，同 ID 覆盖内置）
    ///
    /// 解析失败的文件不会中断流程，但会在控制台输出错误以便排查。
    pub fn load_from_disk(&mut self, app_data_dir: &Path) {
        let dir = app_data_dir.join("personas");
        if !dir.exists() {
            return;
        }
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "yaml" || e == "yml") {
                    match fs::read_to_string(&path) {
                        Ok(content) => match serde_yaml::from_str::<PersonaConfig>(&content) {
                            Ok(persona) => {
                                self.personas.insert(persona.id.clone(), persona);
                            }
                            Err(e) => {
                                eprintln!("[persona] YAML 解析失败 {}: {}", path.display(), e)
                            }
                        },
                        Err(e) => eprintln!("[persona] 文件读取失败 {}: {}", path.display(), e),
                    }
                }
            }
        }
    }

    /// 获取人格配置
    pub fn get_persona(&self, id: &str) -> Option<&PersonaConfig> {
        self.personas.get(id)
    }

    /// 获取默认人格（确定性：优先内置默认 ID，其次任意一个；为空时返回 None）
    pub fn default_persona(&self) -> Option<&PersonaConfig> {
        self.personas
            .get(&self.default_id)
            .or_else(|| self.personas.values().next())
    }

    /// 构建 system prompt（带变量插值 + 用户信息注入）
    ///
    /// 指定人格不存在时确定性回退到内置默认人格。
    pub fn build_system_prompt(
        &self,
        persona_id: &str,
        user_nickname: &str,
        user_info: Option<&crate::config::types::UserConfig>,
    ) -> Result<String> {
        let persona = self
            .get_persona(persona_id)
            .or_else(|| self.default_persona())
            .ok_or_else(|| anyhow::anyhow!("人格引擎为空，无法构建系统提示词"))?;

        let mut prompt = persona.system_prompt.clone();

        // 变量插值
        prompt = prompt.replace("{user_nickname}", user_nickname);

        // 注入用户信息
        if let Some(user) = user_info {
            let mut info_lines = Vec::new();
            if !user.nickname.is_empty() {
                info_lines.push(format!("- 昵称：{}", user.nickname));
            }
            if !user.gender.is_empty() {
                info_lines.push(format!("- 性别：{}", user.gender));
            }
            if !user.birthday.is_empty() {
                info_lines.push(format!("- 生日：{}", user.birthday));
            }
            if !user.pronouns.is_empty() {
                info_lines.push(format!("- 称呼代词：{}", user.pronouns));
            }
            if !user.bio.is_empty() {
                info_lines.push(format!("- 简介：{}", user.bio));
            }

            if !info_lines.is_empty() {
                prompt.push_str("\n\n【用户信息】\n");
                for line in info_lines {
                    prompt.push_str(&format!("{}\n", line));
                }
            }
        }

        // 追加约束
        if !persona.constraints.is_empty() {
            prompt.push_str("\n\n【角色约束】\n");
            for constraint in &persona.constraints {
                prompt.push_str(&format!("- {}\n", constraint));
            }
        }

        Ok(prompt)
    }

    /// 列出所有人格
    pub fn list_personas(&self) -> Vec<&PersonaConfig> {
        self.personas.values().collect()
    }

    /// 列出所有人格 ID
    pub fn list_persona_ids(&self) -> Vec<String> {
        self.personas.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 创建隔离的临时目录（按测试名区分，避免并行冲突）
    fn temp_personas_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "konata-persona-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("personas")).expect("create temp dir");
        dir
    }

    #[test]
    fn default_persona_is_deterministic_builtin() {
        let engine = PersonaEngine::new().expect("engine");
        let default = engine.default_persona().expect("builtin exists");
        assert_eq!(default.id, super::super::types::DEFAULT_PERSONA_ID);
    }

    #[test]
    fn missing_persona_falls_back_to_default_deterministically() {
        let engine = PersonaEngine::new().expect("engine");
        // 即使 HashMap 中存在多个条目（迭代顺序不定），
        // 回退也必须稳定指向内置默认 ID
        let prompt = engine
            .build_system_prompt("不存在的ID", "用户", None)
            .expect("fallback prompt");
        assert!(
            prompt.contains("此方"),
            "回退后应使用内置默认人格的提示词"
        );
    }

    #[test]
    fn build_system_prompt_bails_when_engine_empty() {
        // 构造空引擎：通过反序列化绕过 new() 的内置注入
        let empty = PersonaEngine {
            personas: HashMap::new(),
            default_id: "none".to_string(),
        };
        assert!(empty.build_system_prompt("any", "u", None).is_err());
    }

    #[test]
    fn load_from_disk_merges_overrides_and_skips_invalid() {
        let root = temp_personas_root("merge");
        let dir = root.join("personas");

        // 用户覆盖内置同 ID 人格
        fs::write(
            dir.join("konata-default.yaml"),
            r#"
id: "konata-default"
name: "覆盖版此方"
system_prompt: |
  覆盖后的提示词
"#,
        )
        .unwrap();

        // 新增自定义人格（.yml 扩展名也应支持）
        fs::write(
            dir.join("custom.yml"),
            r#"
id: "custom-y"
name: "YML角色"
system_prompt: |
  yml 提示词
"#,
        )
        .unwrap();

        // 非法 YAML：应被跳过且不影响其他文件
        fs::write(dir.join("broken.yaml"), "system_prompt: [未闭合").unwrap();

        let engine = PersonaEngine::load_all(&root).expect("load_all");

        assert_eq!(
            engine.get_persona("konata-default").unwrap().name,
            "覆盖版此方",
            "用户文件应覆盖内置人格"
        );
        assert!(engine.get_persona("custom-y").is_some(), ".yml 应被加载");
        // broken.yaml 无 id 字段，解析失败被跳过 —— 引擎仍可用
        assert!(engine.get_persona("broken").is_none());
        // 默认回退指向被覆盖后的内置 ID
        assert_eq!(engine.default_persona().unwrap().id, "konata-default");

        let _ = fs::remove_dir_all(&root);
    }
}
