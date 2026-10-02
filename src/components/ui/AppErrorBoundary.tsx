import { Component, type ErrorInfo, type ReactNode } from "react";

interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
}

/**
 * 全局错误边界
 *
 * 任何组件渲染期抛错都会被这里兜住，而不是把整个 WebView 变成白屏
 * （此前 `ToolApprovalDialog` 的注释明确记录过这个风险）。
 *
 * 「重载界面」只刷新当前窗口：聊天状态在 store / 数据库里，刷新即可恢复；
 * 「复制错误详情」让用户能把堆栈贴给开发者。
 */
export class AppErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("Unhandled render error:", error, info.componentStack);
  }

  private handleReload = () => {
    window.location.reload();
  };

  private handleCopy = async () => {
    const { error } = this.state;
    const text = `${error?.name}: ${error?.message}\n${error?.stack ?? ""}`;
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      // 剪贴板不可用时退回选中文本，用户仍可手动复制
      console.error("Copy failed:", text);
    }
  };

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;

    return (
      <div className="app crash-screen" role="alert">
        <div className="crash-card">
          <h1>界面出错了</h1>
          <p>渲染过程中遇到未处理的异常，界面已停止响应。重新加载通常可以恢复。</p>
          <pre className="crash-detail">
            {error.name}: {error.message}
          </pre>
          <div className="crash-actions">
            <button type="button" className="modal-confirm-btn" onClick={this.handleReload}>
              重载界面
            </button>
            <button type="button" className="modal-cancel-btn" onClick={() => void this.handleCopy()}>
              复制错误详情
            </button>
          </div>
        </div>
      </div>
    );
  }
}
