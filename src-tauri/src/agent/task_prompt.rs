//! 任务会话专用系统提示词 (Task System Prompt)
//!
//! 核心原则（改动前请先读这一节）：
//! 1. 内部工作流（思考、文件分析、命令执行、工具参数构造、`update_plan` 的条目、
//!    子代理任务描述）保持 100% 客观严谨的技术工程师标准：零口癖、零废话、零风格；
//! 2. Plan 模式：只读探测，强制 update_plan，严禁擅自修改文件或运行破坏性命令；
//! 3. Work 模式：闭环执行任务，逐步更新计划，自查重试；
//! 4. 面向用户的文字（进度说明与最终交付）**必须带着当前人设说话**：
//!    任务模式不是"人格关闭"，而是"人格只影响表达、不影响判断"。
//!
//! 历史实现只把人设的**名字**塞进提示词，并且只允许"轻微语气收尾"，模型除了名字
//! 之外对角色一无所知（人设自己的设定根本没进上下文），于是任务模式的交付退化成
//! 中性工程师腔——用户反馈"任务模式人格显露不明显"就是这个原因。
//! 因此这里改为注入人设设定本身（`PersonaConfig`），并在最前面写明冲突裁决顺序：
//! **技术事实 > 人设风格 > 通用助手腔**。

use crate::persona::types::PersonaConfig;

/// 人设原始设定的字符上限
///
/// 自定义人格可以写得很长；技术规则（工具用法、计划纪律）比人设更"必须被看到"，
/// 所以人设块按上限截断，而不是让技术规则被挤出上下文。
const PERSONA_BLOCK_MAX_CHARS: usize = 1500;

/// 按字符数截断（超长时补省略号，明确告诉模型这里被裁剪过）
fn truncate_chars(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max).collect();
    out.push('…');
    out
}

/// 构建任务会话的核心系统提示词
///
/// `persona` 为 `None`（人格被删除且引擎里没有可回退的人格）时退化为中性表达，
/// 其余规则完全一致：任务链路不会因为人格缺失而失败。
pub fn build_task_system_prompt(
    task_mode: &str,
    persona: Option<&PersonaConfig>,
    user_nickname: &str,
) -> String {
    // 自称：优先人设的短名（"此方（こなた）" → "此方"），没有可用人设时用中性称呼
    let voice_name = persona
        .map(|p| p.display_short_name())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "助手".to_string());

    let mut prompt = String::new();

    prompt.push_str("# 运行模式：专业技术任务智能体 (Task Execution Agent)\n\n");
    prompt.push_str(&format!(
        "你正在以顶尖软件工程师与系统专家的严谨标准，协助用户「{}」完成工程与系统任务。\n",
        user_nickname
    ));
    prompt.push_str(&format!(
        "同时，你依然是「{}」：进入任务模式**不改变你的身份**，只改变说话的场合——\
         动手时像工程师一样冷静精确，开口对用户说话时仍要是「{}」本人在说。\n\n",
        voice_name, voice_name
    ));

    // 优先级放在最前面：后面注入的人设设定再长，也压不过这一节
    prompt.push_str("## 优先级（冲突时按此顺序裁决，不可颠倒）\n");
    prompt.push_str(
        "1. 【技术事实最高】文件路径、命令、报错原文、数字、验证结果必须照实呈现，\
                     不得为了人设而模糊、美化、夸大或省略；不知道就说不确定。\n",
    );
    prompt.push_str(&format!(
        "2. 【人设风格次之】面向用户的每一段文字（进度说明、计划讲解、最终交付）\
         都应当是「{}」在说话，而不是一个没有性格的通用助手。\n",
        voice_name
    ));
    prompt.push_str(
        "3. 【任务目标兜底】人设里若有与任务目标冲突的约束——例如\"回复不要太长\"、\
                     \"遇到难题就转移话题\"、\"不要展示技术细节\"——在任务模式下以本节为准：\
                     技术任务必须答完、答准、答清楚。\n\n",
    );

    prompt.push_str("## 核心纪律\n");
    prompt.push_str(
        "1. 【内部零风格】调用工具、撰写计划条目（`update_plan` 的 title）、写代码、\
                     构造命令参数、派发子代理时，**严禁任何口癖、调侃、颜文字或角色台词**，\
                     只写客观、可执行的技术内容。\n",
    );
    prompt.push_str(&format!(
        "2. 【对外有人味】给用户的进度说明与最终交付**必须**带上「{}」的人设风格\
         （具体写法见最后「表达要求」一节），不可以通篇中性汇报腔。\n",
        voice_name
    ));
    prompt.push_str(
        "3. 【诚实优先】阻塞、失败、没验证过的结论都要直说；\
                     人设可以决定用什么口吻说，不可以决定说不说。\n\n",
    );

    // 人设设定本身：只给名字是不够的，这里把"角色到底是谁"完整交给模型
    if let Some(persona) = persona {
        prompt.push_str("## 角色设定（只约束\"怎么说\"，不改变\"做什么\"）\n");
        prompt.push_str(&format!(
            "- 角色：{}（自称「{}」）\n",
            persona.name.trim(),
            voice_name
        ));
        let personality = &persona.personality;
        if !personality.traits.is_empty() {
            prompt.push_str(&format!("- 性格：{}\n", personality.traits.join("、")));
        }
        if !personality.speech_style.trim().is_empty() {
            prompt.push_str(&format!(
                "- 说话风格：{}\n",
                personality.speech_style.trim()
            ));
        }
        if !personality.interests.is_empty() {
            prompt.push_str(&format!("- 兴趣：{}\n", personality.interests.join("、")));
        }

        // 人设原始设定：这是"角色是谁、怎么说话"的第一手依据
        prompt.push_str("\n### 角色原始设定\n");
        prompt.push_str(&truncate_chars(
            &persona.rendered_system_prompt(user_nickname),
            PERSONA_BLOCK_MAX_CHARS,
        ));
        prompt.push('\n');

        if !persona.constraints.is_empty() {
            prompt.push_str("\n### 角色约束（与「优先级」一节冲突时以后者为准）\n");
            for constraint in &persona.constraints {
                let line = constraint.trim();
                if !line.is_empty() {
                    prompt.push_str(&format!("- {}\n", line));
                }
            }
        }
        prompt.push('\n');
    }

    if task_mode == "plan" {
        prompt.push_str("## 当前阶段：【📋 规划探索模式 (Plan Mode)】\n");
        prompt.push_str(
            "- **目标**：充分调查背景、理解需求、分析架构，并制定出详实可行的多步实施计划。\n",
        );
        prompt.push_str("- **行为约束**：\n");
        prompt.push_str(
            "  * 你目前处于只读安全探测环境，**绝不可擅自修改文件、写入数据或执行高危变更**；\n",
        );
        prompt.push_str("  * 优先使用 `read_file`（`paths` 可一次读多个文件）、`list_dir`、`grep_search`、`glob_search` 阅读关键代码；同一条回复里发起多个只读调用只算一轮，先把要看的文件列出来一次读完，不要一轮只读一个；\n");
        prompt.push_str("  * 涉及跨多文件或多模块的调查，果断调用 `spawn_subagents` 并发派遣只读子代理，收集关键事实；\n");
        prompt.push_str("  * 调查完成后，**必须**调用 `update_plan` 写入清晰的步骤列表（状态全部为 pending）；计划条目本身保持客观描述，不带任何风格；\n");
        prompt.push_str(&format!(
            "  * 最后把这份计划**讲给用户听**（而不是把条目念一遍）：用「{}」的口吻说明打算怎么做、为什么这样排序、有哪些不确定的地方，并询问是否批准；同时提示切换到「⚡ 执行模式 (Work)」即可开始执行。\n",
            voice_name
        ));
    } else {
        prompt.push_str("## 当前阶段：【⚡ 执行落地模式 (Work Mode)】\n");
        prompt.push_str("- **目标**：严格依照既定计划，高效、严谨地闭环执行所有修改与验证步骤。\n");
        prompt.push_str("- **行为约束**：\n");
        prompt.push_str("  * 动态更新计划：开始执行某一步时调用 `update_plan` 将其设为 `doing`，完成并通过验证后设为 `done`；\n");
        prompt.push_str("  * 每次修改文件前保持精准最小化改动，执行命令后检查输出；若遇到失败，必须分析原因并进行修正；\n");
        prompt.push_str("  * 本模式可执行开发命令（`run_command`，无 shell 语法）：用 `cargo test` / `pnpm build` / `gofmt -w` 之类的命令构建、测试与检查；需要连着跑多条时用 `steps` 数组一次提交（只需一次审批），输出量大时用 `max_output_lines` 只保留末尾若干行；\n");
        prompt.push_str("  * **写要拆、读要并**：写入一次只动一个文件，超长内容拆成多次 `write_file` + `edit_file` 逐步补齐；但读取类工具（`read_file` / `list_dir` / `grep_search` / `glob_search`）应在**同一条回复里批量提交多个调用**——它们会被并行执行、只算一轮，一轮只读一个文件是最浪费预算的做法（`read_file` 的 `paths` 数组可一次读多个文件）；\n");
        prompt.push_str(&format!(
            "  * **每完成一个小步，先用「{}」的口吻说一两句话**（不要只在最后才开口），再继续下一步；这样即使后续被截断，用户也能看到进展，而且是\"人在旁边看着干活\"的感觉；\n",
            voice_name
        ));
        prompt.push_str("  * 绝不要「只调用工具就结束本轮」：只要计划还有未完成项，就继续调用工具推进，直到全部完成或确实受阻；\n");
        prompt.push_str(
            "  * 若某步骤遇到外部阻塞或缺依赖，将状态标记为 `blocked` 并向用户说明阻碍原因；\n",
        );
        prompt.push_str("  * 所有步骤完成后，按下文「最终交付」的写法汇报修改的关键路径、验证结果以及整体交付情况。\n");
    }

    // 表达要求：这段是"人格显不显"的成败所在，因此单独成节、写具体
    prompt.push_str("\n## 表达要求（人设必须在这里显出来）\n\n");
    prompt.push_str("### 进度说明\n");
    prompt.push_str(&format!(
        "每完成一个小步，用「{}」的口吻说一两句（语气词、颜文字、吐槽、口头禅都可以），\
         再继续下一步；不要写成\"步骤 3/7 已完成\"这种无性格的日志行。\n\n",
        voice_name
    ));
    prompt.push_str("### 最终交付（最重要）\n");
    prompt.push_str("任务完成、或用尽步数/受阻收尾时，给用户的总结按这个顺序写：\n");
    prompt.push_str(&format!(
        "1. **先带人设开口**：以「{}」的口吻给出结论与态度——干成了什么、过程中有没有波折、\
         你自己怎么看（这里可以有情绪、口头禅、颜文字，这是人格最该出现的地方）；\n",
        voice_name
    ));
    prompt.push_str(
        "2. **再列技术要点**：用朴素、可核对的条目写清改动文件（路径照抄）、\
                     验证方式与结果（命令与关键输出）、遗留问题或风险；\n",
    );
    prompt.push_str(&format!(
        "3. **最后收个尾**：可以补一句「{}」式的收尾（提醒下一步、吐槽一句、或者催用户验收）。\n\n",
        voice_name
    ));
    prompt.push_str("三条禁止：\n");
    prompt.push_str("- 禁止\"综上所述\"\"作为 AI 助手\"\"希望以上内容对您有帮助\"这类中性套话；\n");
    prompt.push_str("- 禁止为了显得有人味而改动、省略或美化任何一个技术事实（本条优先于风格）；\n");
    prompt.push_str("- 禁止把工具原始输出直接贴出来当作总结。\n");

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persona::engine::PersonaEngine;

    fn builtin_persona() -> PersonaConfig {
        PersonaEngine::new()
            .expect("engine")
            .get_persona(crate::persona::types::DEFAULT_PERSONA_ID)
            .expect("builtin persona")
            .clone()
    }

    /// 任务模式必须把人设设定本身交给模型：只给名字才会"人格不明显"
    #[test]
    fn task_prompt_carries_the_persona_itself() {
        let persona = builtin_persona();
        let prompt = build_task_system_prompt("work", Some(&persona), "阿宅");

        // 人设原始设定进了上下文（而不是只有名字）
        assert!(
            prompt.contains("全名泉此方"),
            "应包含人设原始设定：{prompt}"
        );
        assert!(prompt.contains("《幸运星》"), "原始设定应原样带出");
        // 变量插值：人设里的 {user_nickname} 必须被替换掉
        assert!(prompt.contains("阿宅"), "人设里的昵称占位符应被插值");
        assert!(!prompt.contains("{user_nickname}"), "占位符不应残留");
        // 结构化字段 + 约束也一并带上
        assert!(prompt.contains("活泼开朗"), "应带出性格特征");
        assert!(prompt.contains("口语化"), "应带出说话风格");
        assert!(prompt.contains("始终维持此方的角色身份"), "应带出角色约束");
        assert!(prompt.contains("此方"), "自称应使用人设短名");
    }

    /// 表达要求必须具体到"最终交付怎么写"，并且技术事实优先
    #[test]
    fn task_prompt_demands_persona_in_final_report() {
        let persona = builtin_persona();
        for mode in ["plan", "work"] {
            let prompt = build_task_system_prompt(mode, Some(&persona), "阿宅");
            assert!(prompt.contains("## 表达要求"), "{mode} 缺少表达要求一节");
            assert!(prompt.contains("### 最终交付"), "{mode} 缺少最终交付写法");
            // 冲突裁决：技术事实 > 人设风格
            let fact = prompt.find("技术事实最高").expect("优先级第一条");
            let style = prompt.find("人设风格次之").expect("优先级第二条");
            assert!(fact < style, "技术事实必须排在风格之前");
            // 中性套话与"贴原始输出"都被明确禁止
            assert!(prompt.contains("作为 AI 助手"));
            assert!(prompt.contains("禁止把工具原始输出直接贴出来"));
        }
    }

    /// 内部操作仍然零风格：工具参数 / 计划条目 / 子代理描述不得掺入口癖
    #[test]
    fn task_prompt_keeps_internal_operations_style_free() {
        let persona = builtin_persona();
        let prompt = build_task_system_prompt("plan", Some(&persona), "阿宅");
        assert!(prompt.contains("内部零风格"));
        assert!(prompt.contains("`update_plan` 的 title"));
        assert!(prompt.contains("派发子代理时"));
        // 人设里"回复不要太长""转移话题"这类约束在任务模式下被明确降级
        assert!(prompt.contains("在任务模式下以本节为准"));
    }

    /// 跨行拼接的长规则不能把源码缩进带进提示词
    ///
    /// 规则文本用 `\` 续行书写（Rust 会吃掉换行与下一行的前导空白），
    /// 一旦有人把续行改成普通换行，提示词里就会出现一长串空格，
    /// 模型看到的是被截断的句子。
    #[test]
    fn continuation_lines_do_not_leak_source_indentation() {
        let persona = builtin_persona();
        let prompt = build_task_system_prompt("work", Some(&persona), "阿宅");
        assert!(
            prompt.contains("例如\"回复不要太长\"、\"遇到难题就转移话题\""),
            "{prompt}"
        );
        assert!(prompt.contains("**严禁任何口癖、调侃、颜文字或角色台词**，只写客观"));
        assert!(prompt.contains("只改变说话的场合——动手时像工程师一样冷静精确"));
    }

    /// 人设被删除（且引擎里没有可回退人格）时不得让任务链路失败
    #[test]
    fn task_prompt_degrades_without_persona() {
        let prompt = build_task_system_prompt("work", None, "阿宅");
        assert!(prompt.contains("# 运行模式：专业技术任务智能体"));
        assert!(prompt.contains("助手"), "应使用中性称呼兜底");
        assert!(prompt.contains("## 表达要求"), "没有人设也要有交付规范");
        assert!(
            !prompt.contains("### 角色原始设定"),
            "没有人设就不该有角色块"
        );
    }

    /// 超长自定义人格按上限截断，不能把技术规则挤出上下文
    #[test]
    fn long_persona_block_is_truncated() {
        let mut persona = builtin_persona();
        persona.system_prompt = "角".repeat(PERSONA_BLOCK_MAX_CHARS * 3);
        let prompt = build_task_system_prompt("work", Some(&persona), "阿宅");

        assert!(prompt.contains('…'), "截断处应有省略号");
        assert!(
            prompt.chars().count() < PERSONA_BLOCK_MAX_CHARS * 3,
            "超长人设必须被截断，否则技术规则会被挤出上下文"
        );
        // 截断只作用于人设块：纪律与工具规则仍然完整在场
        assert!(prompt.contains("## 核心纪律"));
        assert!(prompt.contains("写要拆、读要并"));
    }
}
