import { useState, type KeyboardEvent } from "react";
import { useChatStore } from "../../stores/chatStore";
import { ModelBar } from "./ModelBar";

export function InputBox() {
  const [input, setInput] = useState("");
  const sendMessage = useChatStore((s) => s.sendMessage);
  const stopGeneration = useChatStore((s) => s.stopGeneration);
  const isStreaming = useChatStore((s) => s.isStreaming);
  const currentSessionId = useChatStore((s) => s.currentSessionId);
  const sessions = useChatStore((s) => s.sessions);
  const setTaskMode = useChatStore((s) => s.setTaskMode);

  const currentSession = sessions.find((s) => s.id === currentSessionId);
  const isTaskSession = currentSession?.session_type === "task";
  const taskMode = currentSession?.task_mode ?? "plan";

  const handleSend = () => {
    if (!input.trim() || isStreaming || !currentSessionId) return;
    sendMessage(input);
    setInput("");
  };

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
  };

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
              onClick={() => currentSessionId && setTaskMode(currentSessionId, "plan")}
              title="规划模式：只读调查并产出计划，不会修改任何文件"
            >
              📋 规划 (Plan)
            </button>
            <button
              type="button"
              className={`task-mode-btn${taskMode === "work" ? " active work" : ""}`}
              aria-pressed={taskMode === "work"}
              onClick={() => currentSessionId && setTaskMode(currentSessionId, "work")}
              title="执行模式：按计划闭环执行修改、运行与验证"
            >
              ⚡ 执行 (Work)
            </button>
          </div>
        </div>
      )}

      {/* 模型选择条：普通会话与任务会话都有（自动选择按钮只在任务会话出现） */}
      <ModelBar />

      <div className="input-box">
        <textarea
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder={
            currentSessionId
              ? isTaskSession
                ? taskMode === "plan"
                  ? "提出目标或让其分析调查... (Plan 模式下只读不改动)"
                  : "下达执行指令... (Work 模式下自动闭环完成修改与测试)"
                : "输入消息... (Enter 发送)"
              : "请先创建会话"
          }
          disabled={isStreaming || !currentSessionId}
          rows={1}
        />
        {isStreaming ? (
          <button
            onClick={() => stopGeneration()}
            title="停止生成"
            className="send-btn"
          >
            ■
          </button>
        ) : (
          <button
            onClick={handleSend}
            disabled={!input.trim() || !currentSessionId}
            className="send-btn"
          >
            ➤
          </button>
        )}
      </div>
    </div>
  );
}
