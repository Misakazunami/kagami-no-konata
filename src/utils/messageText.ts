/** 从文本中提取 <think> 标签内容，返回 { thinking, content } */
export function extractThinkTags(text: string): {
  thinking: string | null;
  content: string;
} {
  const thinkRegex = /<think>([\s\S]*?)<\/think>/g;
  const thinkParts: string[] = [];
  let cleaned = text;
  let match;
  while ((match = thinkRegex.exec(text)) !== null) {
    thinkParts.push(match[1].trim());
  }
  if (thinkParts.length > 0) {
    cleaned = text.replace(thinkRegex, "").trim();
  }
  return {
    thinking: thinkParts.length > 0 ? thinkParts.join("\n\n") : null,
    content: cleaned,
  };
}

/**
 * 展示 / 导出用的助手正文
 *
 * 优先使用消息自带的 `thinking` 字段；旧数据没有该字段时，
 * 降级为从正文里剥离 `<think>` 标签（与 MessageBubble 同一套规则）。
 */
export function visibleAssistantContent(message: {
  content: string;
  thinking?: string | null;
}): string {
  return message.thinking ? message.content : extractThinkTags(message.content).content;
}
