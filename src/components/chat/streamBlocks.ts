/**
 * 流式 Markdown 的块切分：把"已经写完的段落"与"还在追加的尾部"分开
 *
 * 为什么要切：整段内容每帧重新解析 markdown 的代价随文本线性增长
 * （整场流式累计 O(n²)）；稳定块只解析一次、尾部每帧只解析一小段，
 * 总成本回到 O(n)。切分规则：
 * - 围栏外的空行是块边界（CommonMark 的段落/标题/列表/表格等大多如此）；
 * - 跟踪代码围栏（``` / ~~~，最多 3 个前导空格），围栏内不切分，
 *   保证稳定块里的围栏一定闭合（可以安全开语法高亮）；
 * - 最后一个块始终留在 `tail`，因为它还在增长。
 *
 * 已知取舍：跨空行的有序列表在流式中间态可能重新编号、blockquote 可能
 * 短暂分裂成两段；这些只是过程态，流结束后的完整消息渲染会恢复正确。
 */

/** 最多 3 个前导空格的围栏行（\`\`\` 或 ~~~，≥3 个字符） */
const FENCE_RE = /^\s{0,3}(`{3,}|~{3,})/;

export interface SplitBlocks {
  /** 已写完、内容不会再变的块（顺序与原文一致） */
  stable: string[];
  /** 仍在追加的尾部（可能为空） */
  tail: string;
}

export function splitStableBlocks(text: string): SplitBlocks {
  const lines = text.split("\n");
  const stable: string[] = [];
  let fenceMarker: string | null = null;
  let blockStart = 0;

  for (let i = 0; i < lines.length; i++) {
    const fence = lines[i].match(FENCE_RE);
    if (fence) {
      // opening fence 用哪个字符，就必须用同一字符关闭（` 与 ~ 不互相干扰）
      if (fenceMarker === null) {
        fenceMarker = fence[1][0];
      } else if (fenceMarker === fence[1][0]) {
        fenceMarker = null;
      }
      continue;
    }
    if (fenceMarker === null && lines[i].trim() === "") {
      const block = lines.slice(blockStart, i).join("\n");
      // 保留原始内容（缩进代码块、行尾两个空格的硬换行都靠它），只跳过纯空白块
      if (block.trim()) stable.push(block);
      blockStart = i + 1;
    }
  }

  return { stable, tail: lines.slice(blockStart).join("\n") };
}
