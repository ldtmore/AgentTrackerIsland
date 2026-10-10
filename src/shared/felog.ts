/**
 * 前端日志通道（2026-09-17 埋点审查 P2）：webview 侧错误落文件日志。
 * - felog(level, msg)：手动埋点入口（关键交互失败按需调用）
 * - installFelogs()：全局兜底钩子——未捕获 JS 错误 + 未处理 Promise 拒绝，
 *   main.tsx 挂载时调用一次；窗口名由后端从 Tauri 窗口 label 取，前端不用传
 * 设计约束：
 * - 通道自身任何失败一律静默（后端不可用时不允许级联出新错误）
 * - 节流防刷屏：同一消息 5 秒窗口只记首条（渲染循环错误每帧触发也能扛住，
 *   与后端 20MB 日志滚动共同构成两道闸）
 */

import { invoke } from "@tauri-apps/api/core";

/** 与后端 FRONTEND_LEVELS 白名单对齐 */
export type FelogLevel = "error" | "warn" | "info" | "debug";

/** 消息指纹 → 上次记录时间戳：节流窗口内同消息只记首条 */
const recent = new Map<string, number>();
const DEDUP_MS = 5000;
/** 指纹表容量兜底：超限时删最老条目（错误消息集合有限，简单策略即可） */
const RECENT_MAX = 100;

/** 前端日志落文件（手动埋点与全局钩子共用）；任何失败静默，绝不抛出 */
export function felog(level: FelogLevel, message: string): void {
  try {
    const now = Date.now();
    if (now - (recent.get(message) ?? 0) < DEDUP_MS) return;
    recent.set(message, now);
    if (recent.size > RECENT_MAX) {
      let oldestKey: string | null = null;
      let oldestTs = Infinity;
      for (const [k, ts] of recent) {
        if (ts < oldestTs) {
          oldestTs = ts;
          oldestKey = k;
        }
      }
      if (oldestKey !== null) recent.delete(oldestKey);
    }
    invoke("log_frontend", { level, message }).catch(() => {
      /* 后端不可用：静默 */
    });
  } catch {
    /* 通道自身异常：静默 */
  }
}

/** 全局兜底钩子：未捕获 JS 错误 + 未处理 Promise 拒绝（已被 catch 的不经过这里） */
export function installFelogs(): void {
  window.addEventListener("error", (e) => {
    felog(
      "error",
      `未捕获错误：${e.message} @ ${e.filename}:${e.lineno}:${e.colno}`,
    );
  });
  window.addEventListener("unhandledrejection", (e) => {
    const reason =
      e.reason instanceof Error
        ? (e.reason.stack ?? e.reason.message)
        : String(e.reason);
    felog("error", `未处理的 Promise 拒绝：${reason}`);
  });
}
