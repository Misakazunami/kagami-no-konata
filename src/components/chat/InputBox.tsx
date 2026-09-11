import { useState, type KeyboardEvent } from "react";
import { useChatStore } from "../../stores/chatStore";

export function InputBox() {
  const [input, setInput] = useState("");
  const sendMessage = useChatStore((s) => s.sendMessage);
  const stopGeneration = useChatStore((s) => s.stopGeneration);
  const isStreaming = useChatStore((s) => s.isStreaming);
  const currentSessionId = useChatStore((s) => s.currentSessionId);

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
    <div className="input-box">
      <textarea
        value={input}
        onChange={(e) => setInput(e.target.value)}
        onKeyDown={handleKeyDown}
        placeholder={
          currentSessionId ? "输入消息... (Enter 发送)" : "请先创建会话"
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
  );
}
