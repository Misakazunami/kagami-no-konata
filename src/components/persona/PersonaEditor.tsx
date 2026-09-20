import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useChatStore } from "../../stores/chatStore";
import type { PersonaSummary } from "../../types/persona";

export function PersonaEditor() {
  const [personas, setPersonas] = useState<PersonaSummary[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [yamlContent, setYamlContent] = useState("");
  const [saving, setSaving] = useState(false);
  const [status, setStatus] = useState("");
  const setCurrentPage = useChatStore((s) => s.setCurrentPage);

  // 加载人格列表
  const loadPersonas = async () => {
    try {
      const list = await invoke<PersonaSummary[]>("list_personas");
      setPersonas(list);
      if (list.length > 0 && !selectedId) {
        selectPersona(list[0].id);
      }
    } catch (e) {
      console.error("Failed to load personas:", e);
    }
  };

  useEffect(() => {
    loadPersonas();
  }, []);

  // 选中人格，加载 YAML
  const selectPersona = async (id: string) => {
    setSelectedId(id);
    setStatus("");
    try {
      const yaml = await invoke<string>("get_persona_yaml", { personaId: id });
      setYamlContent(yaml);
    } catch (e) {
      setYamlContent(`# 加载失败: ${e}`);
    }
  };

  // 保存
  const handleSave = async () => {
    if (!selectedId) return;
    setSaving(true);
    setStatus("");
    try {
      await invoke("save_persona", {
        personaId: selectedId,
        yamlContent,
      });
      setStatus("✅ 保存成功");
      await loadPersonas(); // 刷新列表（可能新增了）
    } catch (e) {
      setStatus(`❌ 保存失败: ${e}`);
    } finally {
      setSaving(false);
    }
  };

  // 删除
  const handleDelete = async () => {
    if (!selectedId) return;
    const persona = personas.find((p) => p.id === selectedId);
    if (persona?.is_builtin) {
      setStatus("❌ 内置人格无法删除");
      return;
    }
    // 统计引用该人格的会话数，删除后这些会话将回退为默认人格
    const sessions = useChatStore.getState().sessions;
    const usedCount = sessions.filter((s) => s.persona_id === selectedId).length;
    const confirmMsg =
      usedCount > 0
        ? `确认删除人格「${persona?.name}」？\n该人格被 ${usedCount} 个会话使用，删除后这些会话将回退为默认人格。`
        : `确认删除人格「${persona?.name}」？`;
    if (!confirm(confirmMsg)) return;

    try {
      await invoke("delete_persona", { personaId: selectedId });
      setSelectedId(null);
      setYamlContent("");
      setStatus("✅ 已删除");
      await loadPersonas();
    } catch (e) {
      setStatus(`❌ 删除失败: ${e}`);
    }
  };

  // 新建人格
  const handleNew = () => {
    const newId = `custom-${Date.now()}`;
    const template = `id: "${newId}"
name: "新角色"
version: "1.0.0"

system_prompt: |
  在这里编写角色的系统提示词...
  用 {user_nickname} 来引用用户昵称。

personality:
  traits:
    - "特征1"
    - "特征2"
  speech_style: "说话风格描述"
  interests:
    - "兴趣1"
    - "兴趣2"

constraints:
  - "约束条件1"
  - "约束条件2"

# 以下三项是"角色外观文案"：UI 按钮与桌宠戳一戳台词都会随人格变化。
# 不填也能用（后端会用中性兜底），填了角色感更强。
short_name: "新角色"          # 侧栏按钮与空状态里显示的短名

poke_lines:                   # 戳一戳随机台词
  - "呀！别戳啦～"
  - "嗯？怎么啦？"

poke_angry_line: "不要再戳啦……要生气啦！"   # 连续戳到生气时的台词
`;
    setSelectedId(newId);
    setYamlContent(template);
    setStatus("📝 请编辑后保存");
  };

  return (
    <div className="persona-editor">
      {/* 顶栏 */}
      <div className="persona-header">
        <button className="back-btn" onClick={() => setCurrentPage("settings")}>
          ← 返回设置
        </button>
        <h2>✦ 人格编辑</h2>
      </div>

      <div className="persona-body">
        {/* 左侧列表 */}
        <div className="persona-list">
          <button className="persona-new-btn" onClick={handleNew}>
            + 新建人格
          </button>
          {personas.map((p) => (
            <div
              key={p.id}
              className={`persona-item ${p.id === selectedId ? "active" : ""}`}
              onClick={() => selectPersona(p.id)}
            >
              <span className="persona-item-name">{p.name}</span>
              {p.is_builtin && <span className="persona-badge">内置</span>}
            </div>
          ))}
          {personas.length === 0 && (
            <div className="persona-empty">暂无人格</div>
          )}
        </div>

        {/* 右侧编辑器 */}
        <div className="persona-edit-area">
          {selectedId ? (
            <>
              <textarea
                className="persona-yaml-editor"
                value={yamlContent}
                onChange={(e) => setYamlContent(e.target.value)}
                spellCheck={false}
              />
              <div className="persona-actions">
                <button onClick={handleSave} disabled={saving}>
                  {saving ? "保存中..." : "💾 保存人格"}
                </button>
                <button
                  className="persona-delete-btn"
                  onClick={handleDelete}
                  disabled={personas.find((p) => p.id === selectedId)?.is_builtin}
                >
                  🗑 删除
                </button>
              </div>
              {status && <div className="persona-status">{status}</div>}
            </>
          ) : (
            <div className="persona-empty-hint">
              <p>← 选择一个人格进行编辑</p>
              <p>或点击「新建人格」创建新角色</p>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
