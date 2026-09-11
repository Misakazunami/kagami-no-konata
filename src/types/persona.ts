/**
 * 人格相关的前端类型（唯一来源）
 *
 * 后端 `list_personas` / `get_persona_summary` 都返回这个结构。
 * 角色外观文案（短名、戳一戳台词）由人格 YAML 提供，**随人格变化**：
 * 前端不得再硬编码任何角色名或台词，否则换人格后会串戏。
 */
export interface PersonaSummary {
  id: string;
  /** 完整名，例如「此方（こなた）」 */
  name: string;
  version: string;
  is_builtin: boolean;
  /** 短名，用于按钮与空状态文案，例如「此方」 */
  short_name: string;
  /** 戳一戳预置台词 */
  poke_lines: string[];
  /** 连续戳一戳到"生气"时的台词 */
  poke_angry_line: string;
}

/** 内置默认人格 id（后端 `DEFAULT_PERSONA_ID`，用于判断"是不是默认角色"） */
export const DEFAULT_PERSONA_ID = "konata-default";

/** 人格缺失时的中性兜底（不绑定任何具体角色名） */
export const FALLBACK_SHORT_NAME = "角色";
