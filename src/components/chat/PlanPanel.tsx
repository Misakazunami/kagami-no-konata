import { useState } from "react";
import { useChatStore } from "../../stores/chatStore";
import type { PlanItem } from "../../types/events";
import {
  PLAN_STATUS_ICON,
  PLAN_STATUS_LABEL,
} from "../../types/tools";

/**
 * 任务计划面板
 *
 * 模型用 `update_plan` 写计划，后端每次生成都会把同一份计划注入 system prompt，
 * 所以这里展示的进度就是模型自己看到的进度——长任务（8 轮上限之外的多步工作）
 * 因此对用户是可见、可预期、可以中途叫停的。
 *
 * 渲染规则：
 * - 计划是**模型写的结构化数据**（一行标题 + 状态），不是外部内容，
 *   因此直接用文本渲染即可，不涉及不可信内容的问题；
 * - 折叠状态只显示一行进度摘要（`3/5 · 正在：跑测试`），展开看全部条目。
 */
export function PlanPanel() {
  const plan = useChatStore((s) => s.plan);
  const note = useChatStore((s) => s.planNote);
  const clearPlan = useChatStore((s) => s.clearPlan);

  return <PlanPanelView plan={plan} note={note} onClear={clearPlan} />;
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

/**
 * 纯展示层
 *
 * 与 store 解耦（只吃 props），因此可以在没有 DOM/没有 store 的环境里被渲染测试覆盖；
 * 容器 `PlanPanel` 只负责把 store 接上来。
 */
export function PlanPanelView({
  plan,
  note,
  onClear,
}: {
  plan: PlanItem[] | null;
  note: string | null;
  onClear: () => void;
}) {
  const [expanded, setExpanded] = useState(false);

  if (!plan || plan.length === 0) return null;

  const done = plan.filter((item) => item.status === "done").length;
  const blocked = plan.filter((item) => item.status === "blocked").length;
  const headline = planHeadline(plan);

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
                <span className="plan-item-icon" aria-hidden="true">
                  {PLAN_STATUS_ICON[item.status] ?? "○"}
                </span>
                <span className="plan-item-title">{item.title}</span>
                <span className={`plan-item-status ${item.status}`}>
                  {PLAN_STATUS_LABEL[item.status] ?? item.status}
                </span>
              </li>
            ))}
          </ol>
          {note && <div className="plan-note">备注：{note}</div>}
          <div className="plan-actions">
            <button className="plan-clear-btn" onClick={onClear}>
              清除计划
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
