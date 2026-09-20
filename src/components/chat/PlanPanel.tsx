import { useState } from "react";
import { useChatStore } from "../../stores/chatStore";
import type { PlanItem } from "../../types/events";
import { PLAN_STATUS_ICON, PLAN_STATUS_LABEL } from "../../types/tools";

/**
 * 任务计划面板
 *
 * 模型用 `update_plan` 写计划，后端每次生成都会把同一份计划注入 system prompt，
 * 所以这里展示的进度就是模型自己看到的进度——长任务的多步工作因此对用户
 * 是可见、可编辑、可中途叫停的。
 *
 * 用户也可以直接改这份计划（勾选/改标题/增删）：写回的是同一张表、同一份 JSON，
 * 模型下一轮就会按用户调整后的进度继续。
 *
 * 渲染规则：
 * - 计划是**模型/用户写的结构化数据**（一行标题 + 状态），不是外部内容，
 *   因此直接用文本渲染即可，不涉及不可信内容的问题；
 * - 折叠状态只显示一行进度摘要（`3/5 · 正在：跑测试`），展开看全部条目。
 */
export function PlanPanel() {
  const plan = useChatStore((s) => s.plan);
  const note = useChatStore((s) => s.planNote);
  const clearPlan = useChatStore((s) => s.clearPlan);
  const updatePlanItems = useChatStore((s) => s.updatePlanItems);
  const approvePlan = useChatStore((s) => s.approvePlan);
  const isStreaming = useChatStore((s) => s.isStreaming);
  const currentSessionId = useChatStore((s) => s.currentSessionId);
  const sessions = useChatStore((s) => s.sessions);
  const session = sessions.find((s) => s.id === currentSessionId);
  // 只有任务会话的 Plan 阶段需要"批准并执行"：Work 阶段已经在执行了
  const canApprove =
    session?.session_type === "task" && (session.task_mode ?? "plan") === "plan";

  return (
    <PlanPanelView
      plan={plan}
      note={note}
      onClear={clearPlan}
      onChange={updatePlanItems}
      onApprove={canApprove ? approvePlan : undefined}
      busy={isStreaming}
    />
  );
}

/** 展开时显示的一行摘要（未进行中时退化为"受阻原因 / 全部完成 / 下一项"） */
export function planHeadline(plan: PlanItem[]): string {
  // 空计划没有"进度"可言：否则 done(0) === length(0) 会报成"全部完成"
  if (plan.length === 0) return "";
  const doing = plan.find((item) => item.status === "doing");
  if (doing) return `正在：${doing.title}`;
  const blocked = plan.find((item) => item.status === "blocked");
  if (blocked) return `受阻：${blocked.title}`;
  const done = plan.filter((item) => item.status === "done").length;
  if (done === plan.length) return "全部完成";
  return plan.find((item) => item.status === "pending")?.title ?? "";
}

/** 点击状态图标时的循环：待办 → 进行中 → 已完成 → 待办；受阻点击即重试 */
const NEXT_STATUS: Record<PlanItem["status"], PlanItem["status"]> = {
  pending: "doing",
  doing: "done",
  done: "pending",
  blocked: "pending",
};

/**
 * 纯展示层
 *
 * 与 store 解耦（只吃 props），因此可以在没有 DOM/没有 store 的环境里被渲染测试覆盖；
 * 容器 `PlanPanel` 只负责把 store 接上来。传入 `onChange` 表示允许编辑。
 */
export function PlanPanelView({
  plan,
  note,
  onClear,
  onChange,
  onApprove,
  busy = false,
}: {
  plan: PlanItem[] | null;
  note: string | null;
  onClear: () => void;
  onChange?: (items: PlanItem[], note?: string | null) => void;
  onApprove?: () => void;
  busy?: boolean;
}) {
  const [expanded, setExpanded] = useState(false);
  /** 正在行内编辑标题的条目下标 */
  const [editingIndex, setEditingIndex] = useState<number | null>(null);
  const [draft, setDraft] = useState("");
  /** 底部"添加一条"输入框 */
  const [newTitle, setNewTitle] = useState("");

  if (!plan || plan.length === 0) return null;

  const editable = !!onChange;
  const done = plan.filter((item) => item.status === "done").length;
  const blocked = plan.filter((item) => item.status === "blocked").length;
  const headline = planHeadline(plan);

  const commit = (items: PlanItem[]) => onChange?.(items, note);

  const cycle = (index: number) => {
    if (!editable || busy) return;
    commit(
      plan.map((item, i) =>
        i === index ? { ...item, status: NEXT_STATUS[item.status] } : item
      )
    );
  };

  const remove = (index: number) => {
    if (!editable || busy) return;
    commit(plan.filter((_, i) => i !== index));
  };

  const saveTitle = (index: number) => {
    if (!editable) return;
    const title = draft.trim();
    setEditingIndex(null);
    if (!title || title === plan[index].title) return;
    commit(plan.map((item, i) => (i === index ? { ...item, title } : item)));
  };

  const addItem = () => {
    const title = newTitle.trim();
    if (!editable || busy || !title) return;
    commit([...plan, { title, status: "pending" }]);
    setNewTitle("");
  };

  return (
    <div className={`plan-panel ${blocked > 0 ? "has-blocked" : ""}`}>
      <button
        className="plan-head"
        onClick={() => setExpanded((prev) => !prev)}
        aria-expanded={expanded}
        title={expanded ? "收起计划" : "展开计划"}
      >
        <span className="plan-icon" aria-hidden="true">
          🗒
        </span>
        <span className="plan-progress">
          {done}/{plan.length}
        </span>
        <span className="plan-headline">{headline}</span>
        {blocked > 0 && <span className="plan-blocked-chip">受阻 {blocked}</span>}
        <span className={`plan-arrow ${expanded ? "expanded" : ""}`}>▶</span>
      </button>

      {expanded && (
        <div className="plan-body">
          <ol className="plan-items">
            {plan.map((item, index) => (
              <li key={`${index}-${item.title}`} className={`plan-item ${item.status}`}>
                {editingIndex === index ? (
                  <input
                    className="plan-item-edit"
                    value={draft}
                    autoFocus
                    onChange={(e) => setDraft(e.target.value)}
                    onBlur={() => saveTitle(index)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") saveTitle(index);
                      if (e.key === "Escape") setEditingIndex(null);
                    }}
                  />
                ) : (
                  <>
                    {editable ? (
                      <button
                        type="button"
                        className="plan-item-toggle"
                        onClick={() => cycle(index)}
                        disabled={busy}
                        title={
                          item.status === "blocked"
                            ? "重试该步"
                            : "切换状态：待办 → 进行中 → 已完成"
                        }
                        aria-label={`切换「${item.title}」的状态`}
                      >
                        {PLAN_STATUS_ICON[item.status] ?? "○"}
                      </button>
                    ) : (
                      <span className="plan-item-icon" aria-hidden="true">
                        {PLAN_STATUS_ICON[item.status] ?? "○"}
                      </span>
                    )}
                    <span
                      className="plan-item-title"
                      onDoubleClick={
                        editable
                          ? () => {
                              setEditingIndex(index);
                              setDraft(item.title);
                            }
                          : undefined
                      }
                      title={editable ? "双击编辑标题" : undefined}
                    >
                      {item.title}
                    </span>
                    <span className={`plan-item-status ${item.status}`}>
                      {PLAN_STATUS_LABEL[item.status] ?? item.status}
                    </span>
                    {editable && item.status === "blocked" && (
                      <button
                        type="button"
                        className="plan-item-action"
                        onClick={() => cycle(index)}
                        disabled={busy}
                        title="把该步重新标为待办并继续"
                      >
                        重试
                      </button>
                    )}
                    {editable && (
                      <button
                        type="button"
                        className="plan-item-action danger"
                        onClick={() => remove(index)}
                        disabled={busy}
                        title="删除该条"
                        aria-label={`删除「${item.title}」`}
                      >
                        ×
                      </button>
                    )}
                  </>
                )}
              </li>
            ))}
          </ol>

          {note && <div className="plan-note">备注：{note}</div>}

          {editable && (
            <div className="plan-add-row">
              <input
                className="plan-add-input"
                value={newTitle}
                placeholder="添加一条计划…"
                disabled={busy}
                onChange={(e) => setNewTitle(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") addItem();
                }}
              />
              <button
                type="button"
                className="plan-add-btn"
                onClick={addItem}
                disabled={busy || !newTitle.trim()}
              >
                添加
              </button>
            </div>
          )}

          <div className="plan-actions">
            {onApprove && (
              <button
                type="button"
                className="plan-approve-btn"
                onClick={onApprove}
                disabled={busy}
                title="切换到执行模式，并按这份计划开始执行"
              >
                ✓ 批准并执行
              </button>
            )}
            <button
              type="button"
              className="plan-clear-btn"
              onClick={onClear}
              disabled={busy}
            >
              清除计划
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
