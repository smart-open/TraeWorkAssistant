import { Component, type ErrorInfo, type ReactNode } from 'react';

interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
}

/**
 * 全局渲染错误兜底：子树抛出未捕获异常时展示简洁错误页（含「刷新页面」），
 * 避免整个应用白屏。错误详情上报 console 供排查。
 */
export default class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error('[ErrorBoundary] 渲染异常:', error, '\n组件栈:', info.componentStack);
  }

  render() {
    if (this.state.error) {
      return (
        <div className="flex min-h-screen flex-col items-center justify-center gap-4 bg-slate-50 p-6 text-center dark:bg-zinc-950">
          <div className="text-4xl">😵</div>
          <h1 className="text-lg font-semibold text-slate-800 dark:text-zinc-100">页面出现异常</h1>
          <p className="max-w-md text-sm text-slate-500 dark:text-zinc-400">
            界面渲染时发生未捕获的错误。可尝试刷新页面；若反复出现，请查看控制台日志或反馈问题。
          </p>
          <pre className="max-w-lg max-h-40 overflow-auto rounded-lg bg-slate-100 p-3 text-left text-xs text-rose-600 dark:bg-zinc-900 dark:text-rose-400">
            {this.state.error.message}
          </pre>
          <button
            onClick={() => window.location.reload()}
            className="btn-primary"
          >
            刷新页面
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}
