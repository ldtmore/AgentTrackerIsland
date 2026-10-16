import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { installFelogs } from "./shared/felog";
import { ErrorBoundary } from "./shared/ErrorBoundary";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";

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
    // 连续两轮失败（异常持续 ≥2s）：注入加载提示把长空窗变成可知的等待，
    // 避免被误读为程序故障；单轮失败不显示（竞态边缘防提示 1s 内闪现即逝）
    if (i >= 1) showBootHint();
  }
}

// 懒创建业务窗路由集：仅这些窗口的加载链路全程在用户注视下，长等待期需要
// 加载提示（island／tray-menu／tooltip 常驻隐藏加载、show 时早已就绪，岛
// 胶囊形态也不容许提示文本——均不注入）
const BOOT_HINT_ROUTES = new Set(["#settings", "#report", "#quotas", "#sessions", "#about"]);

/** 窗口路由（与 App.windowRoute 同口径：label 优先，island 回退 hash） */
function bootRoute(): string {
  try {
    const label = getCurrentWebviewWindow().label;
    return label !== "island" ? `#${label}` : window.location.hash;
  } catch {
    // internals 未注入的极端情形退回 hash 判定
    return window.location.hash;
  }
}

/** 注入就绪门等待提示（纯 DOM，复用 App.css 的 .boot-loading 视觉，与
 *  BootLoading 组件同款）：文字用行内中性灰——此阶段 data-theme 尚未挂载、
 *  CSS 变量取深色默认值，浅色系统下首帧底是 #e9ebef 而深档文字对比不足，
 *  固定中性值双底皆可读 */
function showBootHint(): void {
  if (!BOOT_HINT_ROUTES.has(bootRoute())) return;
  if (document.getElementById("boot-hint")) return;
  const el = document.createElement("div");
  el.id = "boot-hint";
  el.className = "boot-loading";
  el.innerHTML =
    '<img class="boot-loading-logo" src="/logo.svg" alt="" />' +
    '<span class="boot-loading-text" style="color:#8a8f98">正在启动…</span>';
  document.body.appendChild(el);
}

/** 移除就绪门提示：渲染前调用（render 同步完成，无中间空隙） */
function removeBootHint(): void {
  document.getElementById("boot-hint")?.remove();
}

// 渲染入口：门自身异常也不得阻断渲染（白屏比降级更糟）
void (async () => {
  try {
    await waitBackendReady();
  } catch {
    /* 门异常：照常渲染 */
  }
  removeBootHint(); // 渲染前移除就绪门提示，与首帧渲染无缝衔接
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      <ErrorBoundary name={location.hash || "island"}>
        <App />
      </ErrorBoundary>
    </React.StrictMode>,
  );
})();
