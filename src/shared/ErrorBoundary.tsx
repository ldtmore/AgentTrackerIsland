/**
 * React 渲染树崩溃兜底（2026-09-17 埋点审查 P2）：
 * 子树渲染抛错时上报文件日志并渲染兜底 UI（替代整窗白屏——白屏无任何线索，
 * 用户只知道"窗口打不开"）。仅捕获渲染期错误；事件回调里的错误走全局
 * onerror 钩子（见 felog.ts）。样式内联：不依赖 CSS 加载成功。
 */
import { Component, type ReactNode } from "react";
import { felog } from "./felog";

interface Props {
  children: ReactNode;
  /** 出错窗口标识（进日志与兜底标题，如 island / settings） */
  name: string;
}

interface State {
  error: Error | null;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: { componentStack: string }) {
    // 渲染崩溃必留痕：消息 + 组件栈（可定位到具体组件）
    felog(
      "error",
      `React 渲染崩溃（${this.props.name}）：${error.message}\n组件栈：${info.componentStack}`,
    );
  }

  render() {
    if (this.state.error) {
      return (
        <div
          style={{
            padding: 24,
            fontFamily: "system-ui, sans-serif",
            background: "#fff",
            color: "#333",
          }}
        >
          <h2 style={{ fontSize: 16 }}>界面出错了（{this.props.name}）</h2>
          <p style={{ fontSize: 13, color: "#888" }}>{this.state.error.message}</p>
          <button
            type="button"
            onClick={() => this.setState({ error: null })}
            style={{ padding: "4px 12px", cursor: "pointer" }}
          >
            重试
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}
