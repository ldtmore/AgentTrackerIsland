/**
 * 跨窗口深链中转（2026-10-16 窗口懒创建配套）：
 * 五个辅助窗口改为打开才创建、关闭即销毁后，目标窗口可能尚未挂载——
 * emit 广播无人接收，深链（跳分区/预过滤/定位实例）会静默丢失。
 * 方案：发起方 emit 照旧（已开窗口的即时通道不变）＋写 localStorage；
 * 新建窗口挂载时 takeDeepLink 读取消费（带时效，命中即清）。
 * 重复消费幂等无害（深链均为滚动定位/筛选类操作）。
 */

const KEY = "ati-deep-link";
const TTL_MS = 15_000;

/** 发起深链：与 emit 并行调用（本函数只负责新建窗口路径的兜底投递） */
export function sendDeepLink(event: string, payload?: unknown): void {
  try {
    localStorage.setItem(KEY, JSON.stringify({ event, payload: payload ?? null, ts: Date.now() }));
  } catch {
    /* localStorage 不可用（极端环境）不阻塞：已开窗口仍有 emit 通道 */
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
