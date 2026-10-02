import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useChatStore } from "../../stores/chatStore";
import { useUiStore } from "../../stores/uiStore";
import { ConfirmDialog } from "../ui/ConfirmDialog";
import { IconChevronLeft, IconSave, IconSparkles, IconTrash } from "../icons";
import type { PersonaSummary } from "../../types/persona";

const toText = (e: unknown) =>
  typeof e === "string" ? e : e instanceof Error ? e.message : String(e);

/** 待二次确认的动作（未保存的编辑不允许被静默丢弃） */
type PendingAction =
  | { kind: "select"; id: string }
  | { kind: "new" }
  | { kind: "back" }
  | { kind: "delete"; id: string; name: string; usedCount: number };

export function PersonaEditor() {
  const [personas, setPersonas] = useState<PersonaSummary[]>([]);
  const [listStatus, setListStatus] = useState<"loading" | "ready" | "error">("loading");
  const [listError, setListError] = useState<string | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [yamlContent, setYamlContent] = useState("");
  /** 上次成功加载/保存的快照：与 `yamlContent` 不一致即为"有未保存修改" */
  const [savedYaml, setSavedYaml] = useState("");
  /**
   * YAML 加载失败标记
   *
   * 关键：失败信息**绝不写进编辑器内容**——否则用户随手点「保存人格」，
   * 错误文本会把原人格文件整份覆盖掉（数据破坏路径）。
   */
  const [loadError, setLoadError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [pending, setPending] = useState<PendingAction | null>(null);
  const setCurrentPage = useChatStore((s) => s.setCurrentPage);
  const pushToast = useUiStore((s) => s.pushToast);
  // 快速连点列表时，只有最后一次请求的结果应该落地
  const loadSeq = useRef(0);

  const dirty = selectedId !== null && yamlContent !== savedYaml;

  const loadPersonas = async () => {
    setListStatus("loading");
    setListError(null);
    try {
      const list = await invoke<PersonaSummary[]>("list_personas");
      setPersonas(list);
      setListStatus("ready");
      if (list.length > 0 && !selectedId) {
        await selectPersona(list[0].id);
      }
    } catch (e) {
      setListStatus("error");
      setListError(toText(e));
    }
  };

  useEffect(() => {
    void loadPersonas();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 选中人格，加载 YAML（失败只置错误标记，不动编辑器内容）
  const selectPersona = async (id: string) => {
    const seq = ++loadSeq.current;
    setSelectedId(id);
    setLoading(true);
    setLoadError(null);
    setYamlContent("");
    setSavedYaml("");
    try {
      const yaml = await invoke<string>("get_persona_yaml", { personaId: id });
      if (seq !== loadSeq.current) return;
      setYamlContent(yaml);
      setSavedYaml(yaml);
    } catch (e) {
      if (seq !== loadSeq.current) return;
      setLoadError(toText(e));
    } finally {
      if (seq === loadSeq.current) setLoading(false);
    }
  };

  const requestSelect = (id: string) => {
    if (id === selectedId) return;
    if (dirty) {
      setPending({ kind: "select", id });
      return;
    }
    void selectPersona(id);
  };

  // 保存
  const handleSave = async () => {
    if (!selectedId || loadError || saving) return;
    setSaving(true);
    try {
      await invoke("save_persona", {
        personaId: selectedId,
        yamlContent,
      });
      setSavedYaml(yamlContent);
      pushToast("人格已保存", "success");
      await loadPersonas(); // 刷新列表（可能新增了）
    } catch (e) {
      pushToast(`保存失败：${toText(e)}`, "error");
    } finally {
      setSaving(false);
    }
  };

  // 删除（统计引用数后走统一确认弹窗）
  const requestDelete = () => {
    if (!selectedId) return;
    const persona = personas.find((p) => p.id === selectedId);
    if (persona?.is_builtin) {
      pushToast("内置人格无法删除", "error");
      return;
    }
    const usedCount = useChatStore
      .getState()
      .sessions.filter((s) => s.persona_id === selectedId).length;
    setPending({
      kind: "delete",
      id: selectedId,
      name: persona?.name ?? selectedId,
      usedCount,
    });
  };

  const doDelete = async (target: { id: string }) => {
    await invoke("delete_persona", { personaId: target.id });
    if (selectedId === target.id) {
      setSelectedId(null);
      setYamlContent("");
      setSavedYaml("");
      setLoadError(null);
    }
    pushToast("已删除人格", "success");
    await loadPersonas();
  };

  // 新建人格
  const handleNew = () => {
    if (dirty) {
      setPending({ kind: "new" });
      return;
    }
    applyNew();
  };

  const applyNew = () => {
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
    loadSeq.current++; // 丢弃仍在途的加载，避免旧人格把模板盖掉
    setSelectedId(newId);
    setLoading(false);
    setLoadError(null);
    setYamlContent(template);
    setSavedYaml("");
    pushToast("已创建草稿，编辑后保存", "info");
  };

  const requestBack = () => {
    if (dirty) {
      setPending({ kind: "back" });
      return;
    }
    setCurrentPage("settings");
  };

  // 确认框回调：ConfirmDialog 会在 onConfirm 成功后自动关闭
  const runPending = async () => {
    const action = pending;
    if (!action) return;
    switch (action.kind) {
      case "select":
        await selectPersona(action.id);
        break;
      case "new":
        applyNew();
        break;
      case "back":
        setCurrentPage("settings");
        break;
      case "delete":
        await doDelete(action);
        break;
    }
  };

  return (
    <div className="persona-editor">
      {/* 顶栏 */}
      <div className="persona-header">
        <button className="back-btn" onClick={requestBack}>
          <IconChevronLeft /> 返回设置
        </button>
        <h2>
          <IconSparkles /> 人格编辑
        </h2>
      </div>

      <div className="persona-body">
        {/* 左侧列表 */}
        <div className="persona-list">
          <button className="persona-new-btn" onClick={handleNew}>
            + 新建人格
          </button>
          {listStatus === "loading" && (
            <div className="persona-empty" role="status">
              正在加载人格…
            </div>
          )}
          {listStatus === "error" && (
            <div className="persona-empty">
              <p>加载失败：{listError}</p>
              <button className="persona-editor-btn" onClick={() => void loadPersonas()}>
                重试
              </button>
            </div>
          )}
          {personas.map((p) => (
            <div
              key={p.id}
              className={`persona-item ${p.id === selectedId ? "active" : ""}`}
              role="button"
              tabIndex={0}
              aria-pressed={p.id === selectedId}
              onClick={() => requestSelect(p.id)}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  requestSelect(p.id);
                }
              }}
            >
              <span className="persona-item-name">{p.name}</span>
              {p.is_builtin && <span className="persona-badge">内置</span>}
            </div>
          ))}
          {listStatus === "ready" && personas.length === 0 && (
            <div className="persona-empty">暂无人格，点击上方按钮创建</div>
          )}
        </div>

        {/* 右侧编辑器 */}
        <div className="persona-edit-area">
          {selectedId ? (
            <>
              {/* 加载失败：错误条 + 禁用保存，绝不把错误文本写进编辑器 */}
              {loadError && (
                <div className="persona-status" role="alert">
                  无法加载该人格：{loadError}
                  <button
                    className="persona-editor-btn"
                    onClick={() => void selectPersona(selectedId)}
                  >
                    重试
                  </button>
                </div>
              )}
              <textarea
                className="persona-yaml-editor"
                value={yamlContent}
                onChange={(e) => setYamlContent(e.target.value)}
                spellCheck={false}
                disabled={loading || !!loadError}
                aria-label="人格 YAML 内容"
                placeholder={loading ? "正在加载…" : ""}
              />
              <div className="persona-actions">
                <button
                  onClick={() => void handleSave()}
                  disabled={saving || loading || !!loadError || !dirty}
                  title={dirty ? "保存修改" : "没有需要保存的修改"}
                >
                  {saving ? "保存中..." : <><IconSave /> 保存人格</>}
                </button>
                <button
                  className="persona-delete-btn"
                  onClick={requestDelete}
                  disabled={
                    saving ||
                    personas.find((p) => p.id === selectedId)?.is_builtin === true
                  }
                >
                  <IconTrash /> 删除
                </button>
                {dirty && !loading && !loadError && (
                  <span className="persona-dirty" role="status">
                    有未保存的修改
                  </span>
                )}
              </div>
            </>
          ) : (
            <div className="persona-empty-hint">
              <p>← 选择一个人格进行编辑</p>
              <p>或点击「新建人格」创建新角色</p>
            </div>
          )}
        </div>
      </div>

      {pending && (
        <ConfirmDialog
          title={
            pending.kind === "delete"
              ? "确认删除人格"
              : pending.kind === "back"
                ? "离开编辑器？"
                : pending.kind === "new"
                  ? "新建人格？"
                  : "切换人格？"
          }
          description={
            pending.kind === "delete" ? (
              pending.usedCount > 0 ? (
                <>
                  将删除人格「{pending.name}」。该人格被{" "}
                  <strong>{pending.usedCount}</strong> 个会话使用，删除后这些会话将回退为默认人格。
                </>
              ) : (
                <>将删除人格「{pending.name}」，且无法恢复。</>
              )
            ) : pending.kind === "back" ? (
              "有未保存的修改，离开后将丢失。"
            ) : pending.kind === "new" ? (
              "当前人格有未保存的修改，新建后将丢失这些修改。"
            ) : (
              "当前人格有未保存的修改，切换后将丢失这些修改。"
            )
          }
          confirmLabel={pending.kind === "delete" ? "确认删除" : "继续"}
          danger={pending.kind === "delete" || pending.kind === "back"}
          onConfirm={runPending}
          onClose={() => setPending(null)}
        />
      )}
    </div>
  );
}
