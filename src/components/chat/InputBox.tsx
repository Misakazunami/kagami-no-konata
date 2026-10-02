import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { useChatStore } from "../../stores/chatStore";
import { ModelBar } from "./ModelBar";
import { IconClipboardList, IconSend, IconStop, IconZap } from "../icons";

/**
 * 各会话的输入草稿（模块级：切页面时 ChatWindow 会整体卸载，草稿不能挂组件里）
 *
 * 草稿按会话隔离：A 会话打一半的内容不会串到 B 会话被误发出去，
 * 也不会因为去设置页转了一圈就丢。
 */
const drafts = new Map<string, string>();

/** 输入框高度自适应上限（与 chat.css 的 max-height 保持一致） */
const MAX_INPUT_HEIGHT = 120;

export function InputBox() {
  const [input, setInput] = useState("");
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const sendMessage = useChatStore((s) => s.sendMessage);
  const stopGeneration = useChatStore((s) => s.stopGeneration);
  const isStreaming = useChatStore((s) => s.isStreaming);
  const stopping = useChatStore((s) => s.stopping);
  const currentSessionId = useChatStore((s) => s.currentSessionId);
  const sessions = useChatStore((s) => s.sessions);
  const setTaskMode = useChatStore((s) => s.setTaskMode);
  const composerPrefill = useChatStore((s) => s.composerPrefill);
  const consumeComposerPrefill = useChatStore((s) => s.consumeComposerPrefill);

  /** 写入草稿（状态 + 会话映射一起更新） */
  const applyInput = (text: string) => {
    setInput(text);
    if (currentSessionId) drafts.set(currentSessionId, text);
  };

  // 高度自适应：多行内容撑到 120px 上限后才出现内部滚动
  const autoGrow = (el: HTMLTextAreaElement | null) => {
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(MAX_INPUT_HEIGHT, el.scrollHeight)}px`;
  };

  // 切换会话：读回该会话的草稿（写入发生在每次 onChange，这里只读）
  useEffect(() => {
    setInput(currentSessionId ? (drafts.get(currentSessionId) ?? "") : "");
    requestAnimationFrame(() => autoGrow(textareaRef.current));
  }, [currentSessionId]);

  // 打开应用 / 切完会话自动聚焦：键盘用户不必先点一下输入框
  useEffect(() => {
    if (!currentSessionId) return;
    const el = textareaRef.current;
    if (el && document.activeElement !== el) el.focus();
  }, [currentSessionId]);

  // 回退一条用户消息后：原文填回输入框并聚焦，方便修改后重发
  useEffect(() => {
    if (!composerPrefill) return;
    applyInput(composerPrefill.text);
    consumeComposerPrefill();
    requestAnimationFrame(() => {
      const el = textareaRef.current;
      if (!el) return;
      el.focus();
      el.setSelectionRange(el.value.length, el.value.length);
      autoGrow(el);
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [composerPrefill, consumeComposerPrefill]);

  const currentSession = sessions.find((s) => s.id === currentSessionId);
  const isTaskSession = currentSession?.session_type === "task";
  const taskMode = currentSession?.task_mode ?? "plan";

  const handleSend = () => {
    // 生成中不发送（输入框仍可编辑，方便先打下一条）；Enter 与按钮同一套判断
    if (!input.trim() || isStreaming || stopping || !currentSessionId) return;
    sendMessage(input);
    applyInput("");
    requestAnimationFrame(() => autoGrow(textareaRef.current));
  };

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    // 输入法组合期间的回车是"确认候选词"，不是发送：
    // 不拦住它，中文/日文用户按回车选词时会把半成品直接发出去。
    // keyCode 229 是 Safari/部分 WebView 在组合态下拿不到 isComposing 时的兜底。
    if (e.nativeEvent.isComposing || e.keyCode === 229) return;
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
  };

  const sendDisabled = !input.trim() || !currentSessionId || stopping;

  return (
    <div className="input-area">
      {/*
        任务会话专属：Plan / Work 模式切换
        与输入框同处一个底栏（共用一条 border-top），不再是"贴在输入框上的一块深色条"
      */}
      {isTaskSession && (
        <div className="task-mode-bar">
          <span className="task-mode-label">运行模式</span>
          <div className="task-mode-group" role="group" aria-label="任务运行模式">
            <button
              type="button"
              className={`task-mode-btn${taskMode === "plan" ? " active plan" : ""}`}
              aria-pressed={taskMode === "plan"}
              disabled={isStreaming}
              onClick={() => currentSessionId && setTaskMode(currentSessionId, "plan")}
              title={
                isStreaming
                  ? "当前生成进行中：模式只影响下一轮，停止或完成后再切换"
                  : "规划模式：只读调查并产出计划，不会修改任何文件"
              }
            >
              <IconClipboardList /> 规划 (Plan)
            </button>
            <button
              type="button"
              className={`task-mode-btn${taskMode === "work" ? " active work" : ""}`}
              aria-pressed={taskMode === "work"}
              disabled={isStreaming}
              onClick={() => currentSessionId && setTaskMode(currentSessionId, "work")}
              title={
                isStreaming
                  ? "当前生成进行中：模式只影响下一轮，停止或完成后再切换"
                  : "执行模式：按计划闭环执行修改、运行与验证"
              }
            >
              <IconZap /> 执行 (Work)
            </button>
          </div>
        </div>
      )}

      {/* 模型选择条：普通会话与任务会话都有（自动选择按钮只在任务会话出现） */}
      <ModelBar />

      <div className="input-box">
        {/*
          生成期间保持可编辑：几乎所有聊天产品都允许"先打下一条"，
          整体 disabled 还会让焦点掉到 body，结束后必须用鼠标重新点回来。
          发送由按钮与 Enter 各自的 isStreaming 判断拦住。
        */}
        <textarea
          ref={textareaRef}
          value={input}
          onChange={(e) => {
            applyInput(e.target.value);
            autoGrow(e.target);
          }}
          onKeyDown={handleKeyDown}
          placeholder={
            currentSessionId
              ? isTaskSession
                ? taskMode === "plan"
                  ? "提出目标或让其分析调查... (Plan 模式下只读不改动)"
                  : "下达执行指令... (Work 模式下自动闭环完成修改与测试)"
                : isStreaming
                  ? "生成中…可先输入下一条 (Enter 发送已暂停)"
                  : "输入消息... (Enter 发送)"
              : "请先创建会话"
          }
          disabled={!currentSessionId}
          rows={1}
          aria-label="消息输入框"
        />
        {isStreaming ? (
          <button
            onClick={() => stopGeneration()}
            title={stopping ? "正在停止…" : "停止生成"}
            className="send-btn"
            disabled={stopping}
            aria-label={stopping ? "正在停止生成" : "停止生成"}
            aria-busy={stopping}
          >
            <IconStop />
          </button>
        ) : (
          <button
            onClick={handleSend}
            disabled={sendDisabled}
            className="send-btn"
            aria-label="发送消息"
            title="发送（Enter）"
          >
            <IconSend />
          </button>
        )}
      </div>
      {stopping && (
        <span className="stopping-hint" role="status">
          正在停止生成…
        </span>
      )}
    </div>
  );
}
