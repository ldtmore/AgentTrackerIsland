/**
 * 跨窗口深链中转（2026-10-16 窗口懒创建配套）：
 * 五个辅助窗口改为打开才创建、关闭即销毁后，目标窗口可能尚未挂载——
 * emit 广播无人接收，深链（跳分区/预筛选/定位实例）会静默丢失。
 * 方案：发起方 emit 照旧（已开窗口的即时通道不变）＋写 localStorage；
 * 新建窗口挂载时 takeDeepLink 读取消费（带时效，命中即清）。
 * 重复消费幂等无害（深链均为滚动定位/筛选类操作）。
 * 2026-10-17 审查 6.4：localStorage 条目加存在性门控——目标窗口已存在时
 * emit 已必达，残留条目会在 15 秒内被「关掉重开的同款窗口」陈旧消费
 * （设置窗开着时点「去设置添加」又手动关掉重开 → 莫名滚动到额度分区）。
 */

import { WebviewWindow } from "@tauri-apps/api/webviewWindow";

const KEY = "ati-deep-link";
const TTL_MS = 15_000;

/** 发起深链：与 emit 并行调用（本函数只负责新建窗口路径的兜底投递）。
 *  target＝目标窗口 label：目标已存在则只依赖 emit、不落 localStorage；
 *  不存在（将走懒创建）才写兜底。枚举经 WebviewWindow.getAll()（@tauri-apps/api
 *  2.x 为 async、内部走 IPC——方案写作同步系 API 事实出入，语义不变）；
 *  目标「创建中」时尚未入列 → 会写 localStorage → 新窗挂载消费，路径正确 */
export async function sendDeepLink(
  event: string,
  target: string,
  payload?: unknown,
): Promise<void> {
  try {
    const exists = (await WebviewWindow.getAll()).some((w) => w.label === target);
    if (exists) return;
    localStorage.setItem(KEY, JSON.stringify({ event, payload: payload ?? null, ts: Date.now() }));
  } catch {
    /* localStorage/窗口枚举不可用（极端环境）不阻塞：已开窗口仍有 emit 通道 */
  }
}

/** 消费深链：目标窗口挂载时调用；无/过期/事件不匹配返回 null，命中即清除 */
export function takeDeepLink<T>(event: string): T | null {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return null;
    const v = JSON.parse(raw) as { event: string; payload: T | null; ts: number };
    if (v.event !== event || Date.now() - v.ts > TTL_MS) return null;
    localStorage.removeItem(KEY);
    return v.payload;
  } catch {
    return null;
  }
}
