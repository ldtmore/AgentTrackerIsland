import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { installFelogs } from "./shared/felog";
import { ErrorBoundary } from "./shared/ErrorBoundary";

// 全局错误兜底钩子先行安装：模块加载期 / 渲染早期的错误也能落文件日志；
// 错误边界包住整树：渲染崩溃落日志并显示兜底 UI（替代白屏），name 取窗口 hash
installFelogs();

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <ErrorBoundary name={location.hash || "island"}>
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
);
