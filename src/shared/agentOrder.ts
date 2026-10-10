/**
 * Agent 用户自定义展示序（2026-10-03 设置页拖拽排序配套）。
 * 真值：app_settings 键 agents_order（JSON 数组，全部 id 的一个置换），由
 * 设置页 AgentSection 拖拽写入；面板筛选菜单/贴边分段条/报表图例等展示位
 * 经本模块消费。
 * - parseAgentOrder：读取侧防御性归一（键缺失/损坏/含未知 id 一律收敛为
 *   「已存序在前＋未覆盖家按 AGENT_DEFS 默认序补尾」的完整列表）；
 * - setAgentOrder／orderAgentIds：模块级顺序注册表（仿 types.ts 的
 *   currentColors 先例），各窗口挂载或收到 agents-changed 时注入，展示
 *   组件直接调用，避免跨层 prop drilling；
 * - parseAgentLimit／setAgentLimit／agentLimit：上岛深度（拖拽序前 N 上岛，
 *   2026-10-08），同一注册表模式；深度过滤只发生在贴边隐藏态分段渲染层。
 * 后端 report_options 有同款归一（Rust 侧 AGENT_DISPLAY_ORDER 常量），
 * 调整默认序时两处同步。
 */
import { AGENT_DEFS } from "./types";

/** 默认序＝AGENT_DEFS 声明序（新 Agent 家落位即在此追加） */
const DEFAULT_ORDER: string[] = AGENT_DEFS.map((a) => a.id);

/** 归一：剔除未知/重复项，缺失家按默认序补尾——恒返回完整合法置换 */
function normalize(order: string[]): string[] {
  const saved = order.filter(
    (id, i) => DEFAULT_ORDER.includes(id) && order.indexOf(id) === i,
  );
  return [...saved, ...DEFAULT_ORDER.filter((id) => !saved.includes(id))];
}

/** 解析设置键原文（JSON 数组）；任何异常按默认序处理（键缺失＝从未拖过） */
export function parseAgentOrder(raw: string | undefined): string[] {
  if (!raw) return [...DEFAULT_ORDER];
  try {
    const v: unknown = JSON.parse(raw);
    if (Array.isArray(v)) {
      return normalize(v.filter((x): x is string => typeof x === "string"));
    }
  } catch {
    /* 损坏键按默认序 */
  }
  return [...DEFAULT_ORDER];
}

/** 运行时顺序注册表（当前窗口内的展示序真值） */
let currentOrder: string[] = [...DEFAULT_ORDER];

export function setAgentOrder(order: string[]) {
  currentOrder = normalize(order);
}

/** 按用户序重排任意 id 子集；未注册的 id 保持传入相对序沉底
 *  （与后端 report_options 的沉底兜底语义一致） */
export function orderAgentIds(ids: string[]): string[] {
  const rank = new Map(currentOrder.map((id, i) => [id, i] as const));
  return [...ids].sort(
    (a, b) =>
      (rank.get(a) ?? Number.MAX_SAFE_INTEGER) -
      (rank.get(b) ?? Number.MAX_SAFE_INTEGER),
  );
}

// ===== 灵动岛上岛深度（2026-10-08「在岛展示前 N 个」） =====
// 拖拽序前 N 个 Agent 上岛（贴边隐藏态分段只渲染这些），其余照常采集不上岛；
// 全局聚合（出错描边/胶囊文案）不过滤——界外 Agent 出错仍由容器红边＋文案兜底。
// 上限 6＝左右隐藏态药丸列满配容量（6×4px＋5×2px gap ≤ 48px 端帽可用区），
// 任何配置下分段不溢出；默认 5（2026-10-08 所有者拍板的行为变化：升级后键缺失
// 即按 5 生效，启用多于 5 个的用户可在设置页调大）

/** 上岛数量上限（与 EdgeTab 药丸列容量同源，改动需两侧同步） */
export const AGENT_LIMIT_MAX = 6;

/** 缺省/损坏/越界一律归一的默认值 */
export const AGENT_LIMIT_DEFAULT = 5;

/** 解析设置键原文（整数字符串）；范围外或非整数回默认 5 */
export function parseAgentLimit(raw: string | undefined): number {
  const v = raw == null ? NaN : Number(raw);
  return Number.isInteger(v) && v >= 1 && v <= AGENT_LIMIT_MAX
    ? v
    : AGENT_LIMIT_DEFAULT;
}

/** 运行时深度注册表（当前窗口内的上岛深度真值），与展示序注册表同模式 */
let currentLimit: number = AGENT_LIMIT_DEFAULT;

export function setAgentLimit(limit: number) {
  currentLimit = parseAgentLimit(String(limit));
}

/** 当前窗口的上岛深度（EdgeTab 分段过滤消费） */
export function agentLimit(): number {
  return currentLimit;
}
