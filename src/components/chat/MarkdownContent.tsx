import { memo, useRef, useState, type HTMLAttributes, type ReactNode } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import { openUrl } from "@tauri-apps/plugin-opener";
import { copyText } from "../../utils/clipboard";

/**
 * 外链渲染：交给系统默认浏览器打开
 *
 * 默认的 `<a>` 会让整个应用 webview 直接导航到外部页面（界面丢失，
 * 且外部页面会运行在注入了 IPC 的 webview 里）。这里拦截点击并调用
 * opener 插件，同时补上 rel="noreferrer"。
 */
function MarkdownLink({
  href,
  children,
  ...rest
}: React.AnchorHTMLAttributes<HTMLAnchorElement>) {
  return (
    <a
      {...rest}
      href={href}
      rel="noreferrer noopener"
      onClick={(event) => {
        event.preventDefault();
        if (!href) return;
        if (!/^https?:\/\//i.test(href)) return; // 只放行 http(s)
        openUrl(href).catch((e) => console.error("打开链接失败:", e));
      }}
    >
      {children}
    </a>
  );
}

/**
 * 代码块渲染：右上角提供"复制"按钮
 *
 * `innerText` 取的是高亮后的纯文本（不含 span 标签），与用户看到的一致；
 * 不展开 react-markdown 传入的 `node` 等额外属性，避免 React 未知 prop 警告。
 */
function CopyablePre({ children, className }: HTMLAttributes<HTMLPreElement>) {
  const preRef = useRef<HTMLPreElement>(null);
  const [copied, setCopied] = useState(false);

  const handleCopy = async () => {
    const text = preRef.current?.innerText ?? "";
    if (!text) return;
    await copyText(text);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };

  return (
    <div className="code-block">
      <button
        type="button"
        className="code-copy-btn"
        onClick={handleCopy}
        title="复制代码"
        aria-label="复制代码"
      >
        {copied ? "已复制" : "复制"}
      </button>
      <pre ref={preRef} className={className}>
        {children as ReactNode}
      </pre>
    </div>
  );
}

// 模块级常量：引用稳定，避免每次渲染重建插件数组（memo 生效的前提之一）
const REMARK_PLUGINS = [remarkGfm];
const REHYPE_PLUGINS = [rehypeHighlight];
const NO_REHYPE_PLUGINS: [] = [];
const MARKDOWN_COMPONENTS = { a: MarkdownLink, pre: CopyablePre } as const;

interface Props {
  content: string;
  /**
   * 是否执行代码高亮。流式渲染的"尾部未闭合块"传 false：
   * 半截代码每帧重新高亮既昂贵又会让颜色抖动，闭合成为稳定块后再高亮。
   */
  highlight?: boolean;
}

function MarkdownContentImpl({ content, highlight = true }: Props) {
  return (
    <Markdown
      remarkPlugins={REMARK_PLUGINS}
      rehypePlugins={highlight ? REHYPE_PLUGINS : NO_REHYPE_PLUGINS}
      components={MARKDOWN_COMPONENTS}
    >
      {content}
    </Markdown>
  );
}

/**
 * 全应用统一的 Markdown 渲染入口（消息气泡与流式气泡共用）
 *
 * memo 化：props 只有内容字符串与开关，内容不变时跳过 remark/highlight 重解析。
 */
export const MarkdownContent = memo(MarkdownContentImpl);
