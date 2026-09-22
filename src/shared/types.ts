/**
 * 与 Rust 侧 serde 输出保持一致的快照类型（state/service.rs）
 */

export type SessionState =
  | "working"
  | "idle"
  | "waiting"
  | "error"
  | "offline";

export interface SessionView {
  id: string;
  agent: string;
  model: string | null;
  project_dir: string | null;
  title: string | null;
  state: SessionState;
  /** 总消耗 = 四项相加（与官方账单同口径，含缓存） */
  session_tokens: number;
  /** 四项拆解（tooltip 展示用） */
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  /** 最近一次调用出错的类型（仅 state=error 时展示原因） */
  error_type: string | null;
  last_activity_at: number | null;
}

export interface QuotaView {
  provider: string;
  window_kind: string;
  used_percent: number | null;
  reset_at: number | null;
}

export type IslandStateName =
  | "no_sessions"
  | "all_idle"
  | "any_working"
  | "any_waiting"
  | "any_error";

export interface IslandSnapshot {
  sessions: SessionView[];
  island: IslandStateName;
  quotas: QuotaView[];
  /** GLM 5h 额度已耗尽（100%）：胶囊/标签据此变红，但会话状态保持真实值 */
  quota_exhausted: boolean;
  /** 采集源连续失败（2026-09-17 审查新增）：岛收缩态据此提示"采集异常" */
  degraded?: boolean;
  /** 今日 token 总量（本机今日零点起，账单口径含缓存） */
  today_tokens: number;
  /** 今日模型调用次数 */
  today_calls: number;
  /** GLM 凭据是否已配置（false 时胶囊展示"额度未配置"而非不展示） */
  glm_configured: boolean;
  generated_at: number;
}

/** 额度提醒阈值（设置页存储，前端启动时加载；默认 80/95） */
export interface Thresholds {
  warn: number;
  danger: number;
}

/** 可监控的 Agent 定义（设置页选择项；均有已实装的适配器） */
export const AGENT_DEFS: {
  id: string;
  label: string;
  /** 系统默认身份色（用户可在设置页自定义） */
  color: string;
}[] = [
  { id: "zcode", label: "ZCode", color: "#34d399" },
  { id: "claude-code", label: "Claude Code", color: "#f59e0b" },
  { id: "codex", label: "Codex", color: "#38bdf8" },
  { id: "kimi-code", label: "Kimi Code", color: "#f472b6" },
  { id: "opencode", label: "OpenCode", color: "#22d3ee" },
  { id: "mimo-code", label: "MiMo Code", color: "#fb923c" },
  { id: "gemini", label: "Gemini CLI", color: "#4285f4" },
  { id: "qwen-code", label: "Qwen Code", color: "#a78bfa" },
];

/** 支持 hooks 增强档的 Agent（与 Rust 侧 HOOKS_AGENTS 同源，设置页按此渲染行内精确开关） */
export const HOOKS_AGENTS = ["claude-code", "codex", "kimi-code", "gemini", "qwen-code"];

/** Agent 默认身份色（id → 颜色；可被用户自定义覆盖） */
export const AGENT_COLORS: Record<string, string> = Object.fromEntries(
  AGENT_DEFS.map((a) => [a.id, a.color]),
);

/** 用户自定义色注册表（运行时由 App 从设置注入；颜色 = 身份，状态用亮度/动效表达） */
let currentColors: Record<string, string> = {};

export function setAgentColors(colors: Record<string, string>) {
  currentColors = colors;
}

const FALLBACK_COLORS = ["#a78bfa", "#38bdf8", "#f472b6", "#facc15", "#4ade80", "#fb7185"];

/** 取 Agent 身份色：用户自定义 → 默认表 → 未知 Agent 按名字散列稳定分配 */
export function agentColor(agent: string): string {
  const custom = currentColors[agent];
  if (custom) return custom;
  const known = AGENT_COLORS[agent];
  if (known) return known;
  let h = 0;
  for (let i = 0; i < agent.length; i++) h = (h * 31 + agent.charCodeAt(i)) >>> 0;
  return FALLBACK_COLORS[h % FALLBACK_COLORS.length];
}

/** 会话状态 → 状态点 class 与中文标签 */
export const SESSION_META: Record<SessionState, { dot: string; label: string }> = {
  working: { dot: "dot-green breathe", label: "工作中" },
  idle: { dot: "dot-green", label: "空闲" },
  waiting: { dot: "dot-amber", label: "等待输入" },
  error: { dot: "dot-red pulse", label: "出错" },
  offline: { dot: "dot-gray", label: "离线" },
};

/** token 数值缩写（K/M/B；含缓存后总量可上 G 级） */
export function fmtTokens(n: number): string {
  if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(1)}B`;
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${Math.round(n / 1_000)}K`;
  return String(n);
}

/** 工作时长（模型生成时长合计）：中文分级显示；
 *  null 显示 —（该 Agent 的转录文件无时长字段，如 Claude Code） */
export function fmtDuration(ms: number | null): string {
  if (ms == null) return "—";
  if (ms < 60_000) return `${Math.round(ms / 1000)} 秒`;
  if (ms < 3_600_000) return `${Math.floor(ms / 60_000)} 分 ${Math.round((ms % 60_000) / 1000)} 秒`;
  if (ms < 86_400_000) return `${Math.floor(ms / 3_600_000)} 时 ${Math.round((ms % 3_600_000) / 60_000)} 分`;
  return `${Math.floor(ms / 86_400_000)} 天 ${Math.round((ms % 86_400_000) / 3_600_000)} 时`;
}

/** 相对时间文案（刚刚/N 分钟前/N 小时前/N 天前），给"空闲/已结束"配上时间量感 */
export function fmtRelative(ts: number | null): string {
  if (ts == null) return "";
  const diff = Date.now() - ts;
  if (diff < 0) return "刚刚";
  if (diff < 60_000) return "刚刚";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} 分钟前`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)} 小时前`;
  return `${Math.floor(diff / 86_400_000)} 天前`;
}

/** 错误类型 → 中文原因（error_type 是各 Agent 上报的自由字符串，关键词归大类） */
export function errorReason(errorType: string | null): string {
  if (!errorType) return "出错";
  const e = errorType.toLowerCase();
  if (e.includes("rate") || e.includes("limit") || e.includes("频率") || e.includes("限流"))
    return "出错 · 限流";
  if (e.includes("quota") || e.includes("billing") || e.includes("credit") || e.includes("额度"))
    return "出错 · 额度/计费";
  if (e.includes("auth") || e.includes("permission") || e.includes("key"))
    return "出错 · 鉴权";
  if (e.includes("timeout") || e.includes("timed") || e.includes("超时")) return "出错 · 超时";
  if (e.includes("network") || e.includes("connect")) return "出错 · 网络";
  if (e.includes("overloaded")) return "出错 · 服务过载";
  return "出错 · API 异常";
}

/** 额度窗口标签：'5h' → '5h'，'weekly' → '7d'。
 *  官方定义（docs.bigmodel.cn Coding Plan 用量说明）：周积分为"自套餐下单时起
 *  以 7 天为一个周期刷新"——是 7 天滚动周期而非自然周，故标 7d 更准确 */
export function windowLabel(kind: string): string {
  if (kind === "5h") return "5h";
  if (kind === "weekly") return "7d";
  return kind;
}

/** 中文时长（悬浮提示用，消除"1h48m"类缩写的歧义） */
export function fmtCountdownCN(resetAt: number | null): string {
  if (resetAt == null) return "";
  const diff = resetAt - Date.now();
  if (diff <= 0) return "即将刷新";
  const m = Math.floor(diff / 60_000);
  const h = Math.floor(m / 60);
  const d = Math.floor(h / 24);
  if (d > 0) return `${d} 天 ${h % 24} 小时`;
  if (h > 0) return `${h} 小时 ${m % 60} 分钟`;
  return `${m} 分钟`;
}

/**
 * 选"最紧张"的额度窗口（2026-09-18 展示改造 C4/E3）：
 * 用量百分比最高者优先——修掉"5h 刚重置 0% 全绿、周窗口快满却无处可看"的盲区。
 * 并列时取窗口更短的（先到期的更紧急）。无额度数据返回 null
 */
export function tensestQuota(quotas: QuotaView[]): QuotaView | null {
  const usable = quotas.filter((q) => q.used_percent != null);
  if (usable.length === 0) return null;
  return usable.reduce((a, b) => {
    if (b.used_percent! !== a.used_percent!) {
      return b.used_percent! > a.used_percent! ? b : a;
    }
    return b.window_kind === "5h" ? b : a;
  });
}

/**
 * 额度百分比对应的警示等级（阈值来自设置页，R5；静默提醒，不弹窗不出声）。
 * 2026-09-20 自 IslandBar 挪入共享模块：托盘菜单额度行复用同一判定，避免阈值语义分叉
 */
export function quotaLevel(
  pct: number,
  warn: number,
  danger: number,
): "normal" | "warn" | "danger" {
  if (pct >= danger) return "danger";
  if (pct >= warn) return "warn";
  return "normal";
}
