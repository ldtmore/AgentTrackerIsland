/**
 * 统一开窗入口（2026-10-16 审查 #9「操作必有回应」专项）：此前各处
 * invoke("show_xxx_window").catch(() => {}) 失败静默——窗口创建失败、系统
 * 异常时点击毫无反应，用户无法区分「没点上」还是「坏了」。
 * helper 只做语义命名并返回原始 Promise，由调用方决定反馈载体（各窗口
 * toast 体系不同，helper 不依赖具体窗口的 toast 实例）；成功静默——
 * 窗口出现本身就是反馈，失败由调用方以本窗口的反馈通道提示。
 */
import { invoke } from "@tauri-apps/api/core";

export function openWindow(cmd: string): Promise<void> {
  return invoke<unknown>(cmd).then(() => undefined);
}
