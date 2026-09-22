/**
 * 会话展示口径（M1-10 从岛面板 Panel.tsx 抽出共享）：
 * 「已结束」判定、卡片标题回退链、活跃优先排序——岛面板与会话窗口共用同一套，
 * 保证同一个会话在两处的状态文案/颜色一致（口径漂移=用户眼中的数据错误）
 */
import type { SessionState } from "./types";
import { SESSION_META } from "./types";

/** Agent 徽标文字（岛面板卡片与会话窗口表格共用） */
export const AGENT_BADGE: Record<string, string> = {
  "claude-code": "CC",
  zcode: "ZC",
};

/** 排序：活跃状态（working/waiting/error）优先，其次按最近活动降序（岛面板卡片用） */
export const ACTIVE_FIRST: Record<string, number> = {
  error: 0,
  waiting: 1,
  working: 2,
  idle: 3,
  offline: 4,
};

/** 空闲超过该时长即按"已结束"展示（P5：历史会话≠空闲，进程级存活信号套在
 *  每个历史会话头上导致满屏假"空闲"——前端按时长近似修正，根治需会话级归属；
 *  Rust 侧 store::ENDED_AFTER_MS 同值同义，两处改动必须同步） */
export const ENDED_AFTER_MS = 2 * 3_600_000;

/** 卡片展示态：真实状态 + 前端修正后的标签/状态灯 */
export interface DisplayState {
  label: string;
  dot: string;
  ended: boolean;
}

/** 宽松解析状态字符串（会话窗口从 SQLite 拿到的是任意文本，非法值按 offline 处理） */
export function toSessionState(raw: string | null): SessionState {
  if (raw === "working" || raw === "idle" || raw === "waiting" || raw === "error") {
    return raw;
  }
  return "offline";
}

/** 展示状态判定：活跃三态原样；offline=已结束；idle 超时修正为已结束（保留真实值，仅改文案） */
export function displayState(state: SessionState, lastActivityAt: number | null): DisplayState {
  // 活跃三态原样展示（working/waiting/error 不修正）
  if (state !== "idle" && state !== "offline") {
    return { ...SESSION_META[state], ended: false };
  }
  // offline = 进程已退出 → 会话已结束
  if (state === "offline") {
    return { label: "已结束", dot: "dot-gray", ended: true };
  }
  // idle：进程还开着；但超过 2 小时无活动按"已结束"展示
  const stale = lastActivityAt != null && Date.now() - lastActivityAt > ENDED_AFTER_MS;
  return stale
    ? { label: "已结束", dot: "dot-gray", ended: true }
    : { label: "空闲", dot: "dot-green", ended: false };
}

/** 卡片/行主文案：标题优先，缺失回退项目名，再回退 id（P3/P9） */
export function cardTitle(title: string | null, projectDir: string | null, id: string): string {
  return title ?? projectDir?.split(/[\\/]/).filter(Boolean).pop() ?? id;
}

/** 岛面板排序：活跃优先 + 最近活动降序（会话窗口走 SQL 排序，不用这个） */
export function sortSessions<T extends { state: SessionState; last_activity_at: number | null }>(
  list: T[],
): T[] {
  return [...list].sort((a, b) => {
    const ra = ACTIVE_FIRST[a.state] ?? 9;
    const rb = ACTIVE_FIRST[b.state] ?? 9;
    if (ra !== rb) return ra - rb;
    return (b.last_activity_at ?? 0) - (a.last_activity_at ?? 0);
  });
}

/** 历史区内部排序（M2-UX-1）：活跃/已结束分流之后「活跃优先」已无语义——
 *  原实现把底层状态权重（idle=3 / offline=4）带进历史区，导致「进程没开的
 *  Agent 全体会话被钉死在队尾」（实测 CC 45 个会话被 41 个 idle 型 ZC 压制，
 *  再被截断挡住 = 用户眼中的"面板漏了 CC"）。改纯最近活动降序；
 *  同毫秒批量写入时按 token 量降序兜底，防 30s 定时重渲染卡片跳位 */
export function sortHistory<
  T extends { last_activity_at: number | null; session_tokens?: number },
>(list: T[]): T[] {
  return [...list].sort(
    (a, b) =>
      (b.last_activity_at ?? 0) - (a.last_activity_at ?? 0) ||
      (b.session_tokens ?? 0) - (a.session_tokens ?? 0),
  );
}
