import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { installFelogs } from "./shared/felog";
import { ErrorBoundary } from "./shared/ErrorBoundary";
import { invoke } from "@tauri-apps/api/core";

// 全局错误兜底钩子先行安装：模块加载期 / 渲染早期的错误也能落文件日志；
// 错误边界包住整树：渲染崩溃落日志并显示兜底 UI（替代白屏），name 取窗口 hash
installFelogs();

/**
 * 后端就绪门（2026-10-16 排查「报表窗口 state not managed」）：本应用 7 个
 * 窗口均为常驻窗口，随应用启动即创建加载，页面挂载期的 invoke 可能早于 Rust
 * setup 完成——State 未注册时报 "state not managed"；早期 IPC 管道未就绪时
 * 更会无声悬挂、promise 永不落定（2026-09-17 实测，报错重试对它无效）。
 * 故在渲染前统一等后端就绪：探测带 store State 的 get_settings（单条轻查询，
 * 毫秒级），每次尝试与 1s 超时竞速（悬挂视同未就绪），1s 间隔重试；60 轮
 * 上限后放行渲染——真故障不该无限卡白屏，由各页面自身的错误态诚实呈现。
 * ⚠ 契约：Rust setup 中所有 State 的 manage 必须先于本门探测通过线完成
 * （现集中于 lib.rs manage(store) 紧邻处）；新增 State 的注册不得下移到
 * setup 尾部，否则依赖它的挂载命令将重新暴露在本竞争之下
 */
async function waitBackendReady(): Promise<void> {
  for (let i = 0; i < 60; i++) {
    if (i > 0) await new Promise((r) => setTimeout(r, 1000));
    const ok = await Promise.race([
      invoke("get_settings").then(
        () => true,
        () => false,
      ),
      // 超时竞速：悬挂（永不落定）与报错同视作未就绪，下一轮重试
      new Promise<boolean>((r) => setTimeout(() => r(false), 1000)),
    ]);
    if (ok) return;
  }
}

// 渲染入口：门自身异常也不得阻断渲染（白屏比降级更糟）
void (async () => {
  try {
    await waitBackendReady();
  } catch {
    /* 门异常：照常渲染 */
  }
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      <ErrorBoundary name={location.hash || "island"}>
        <App />
      </ErrorBoundary>
    </React.StrictMode>,
  );
})();
