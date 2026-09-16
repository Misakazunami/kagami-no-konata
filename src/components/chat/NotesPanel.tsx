import { useState } from "react";
import { useChatStore } from "../../stores/chatStore";
import type { SessionNote } from "../../types/events";

/**
 * 工作记忆面板
 *
 * 模型用 `save_note` 主动记下跨轮结论（"已经查明了什么"），这里把它摊开给用户看，
 * 并提供一键清空——跨轮记忆是唯一会持久影响后续对话的东西，用户必须能看见、能抹掉。
 */
export function NotesPanel() {
  const notes = useChatStore((s) => s.notes);
  const clearNotes = useChatStore((s) => s.clearNotes);

  return <NotesPanelView notes={notes} onClear={clearNotes} />;
}

/** 折叠状态的一行摘要：优先显示第一条的标题 */
export function notesHeadline(notes: SessionNote[]): string {
  if (notes.length === 0) return "";
  const first = notes[0];
  const label = first.title?.trim() || first.content.split("\n")[0].trim();
  const short = label.length > 40 ? `${label.slice(0, 40)}…` : label;
  return notes.length === 1 ? short : `${short} 等 ${notes.length} 条`;
}

/** 纯展示层（与 store 解耦，便于渲染测试） */
export function NotesPanelView({
  notes,
  onClear,
}: {
  notes: SessionNote[];
  onClear: () => void;
}) {
  const [expanded, setExpanded] = useState(false);

  if (notes.length === 0) return null;

  return (
    <div className="notes-panel">
      <button
        className="notes-head"
        onClick={() => setExpanded((prev) => !prev)}
        aria-expanded={expanded}
        title={expanded ? "收起工作记忆" : "展开工作记忆"}
      >
        <span className="notes-icon" aria-hidden="true">
          🧠
        </span>
        <span className="notes-count">工作记忆 {notes.length}</span>
        <span className="notes-headline">{notesHeadline(notes)}</span>
        <span className={`notes-arrow ${expanded ? "expanded" : ""}`}>▶</span>
      </button>

      {expanded && (
        <div className="notes-body">
          <p className="notes-hint">
            模型主动记下的结论，会在之后几轮作为背景资料注入（带「不可信」标记）。
            它们只影响模型的措辞与判断，不会改变工具权限或审批。
          </p>
          <ul className="notes-list">
            {notes.map((note) => (
              <li key={note.id} className="notes-item">
                <span className="notes-item-title">
                  {note.title?.trim() || "（无标题）"}
                </span>
                <pre className="notes-item-content">{note.content}</pre>
              </li>
            ))}
          </ul>
          <div className="notes-actions">
            <button className="notes-clear-btn" onClick={onClear}>
              清空工作记忆
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
