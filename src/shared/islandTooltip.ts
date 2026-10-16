/**
 * 岛悬停气泡的结构化载荷（第七轮审查 4.1 品牌化重做）：
 * 岛的深化信息（状态对账／实例额度明细／今日与历史口径）原先靠系统原生
 * title 气泡承载——白底黑字与全应用深色玻璃语言割裂、只能纯文本排版。
 * 重做后由独立 tooltip 窗口渲染结构化气泡，本模块是两侧共享的数据协议：
 * 岛窗口（App.tsx 悬停引擎）在悬停到期时组装 payload emit 过去，
 * tooltip 窗口（TooltipWindow）按结构渲染。
 *
 * 组装逻辑与被退役的 quotaOverviewTitle（IslandBar 纯文本版）完全同源：
 * 实例全量（in_island 过滤）× 窗口固定排序（5h→7d→其他）× ⚠ 告警档位 ×
 * 余额警戒线说明——只是从「拼纯文本行」改为「产结构化字段」，
 * mono 数字段／附注段／档位色的排版决定权交给渲染层。
 */
import type { IslandSnapshot, IslandAccountView, Thresholds } from "./types";
import {
  accountLabel,
  balanceWarnFor,
  fmtTokens,
  quotaLevel,
  resetPhrase,
  tensestAccount,
  windowLabel,
} from "./types";
import { fmtMoney } from "./format";
import { displayState } from "./sessionDisplay";

/** 单条数据行：label 行首标签（「余额」）／mono 数字段（档位色）／post 附注段 */
export interface TooltipLine {
  /** 档位：normal=主文本色、warn=琥珀、danger=红；非 normal 同时驱动 ⚠ 前缀 */
  level: "normal" | "warn" | "danger";
  /** 行首标签（弱色）；窗口行无标签传 null */
  label: string | null;
  /** 数字段（窗口标＋百分比／金额，mono 渲染） */
  mono: string;
  /** 附注段（重置短语／警戒线说明，弱色渲染）；无附注传 null */
  post: string | null;
}

/** 单实例分组：色点＋标题行下挂数据行（余额行在窗口行之前，与纯文本版同序） */
export interface TooltipGroup {
  /** 品牌色（色点，与托盘明细色点同源） */
  color: string;
  /** 标题：「GLM · 别名」（accountLabel 口径，别名以厂商名开头只显别名） */
  title: string;
  /** 余额行（无余额实例为 null） */
  balance: TooltipLine | null;
  /** 窗口口径行（5h→7d→其他固定序） */
  rows: TooltipLine[];
  /** 无数据引导行（实例既无余额也无额度快照时非 null） */
  empty: string | null;
}

/** 气泡结构化载荷（emit_to("tooltip", "island-tooltip") 的 data 字段） */
export interface TooltipPayload {
  /** 聚合状态行：「有会话正在工作 · 共 138（已结束 137）」（无会话只显状态词） */
  status: string;
  /** 采集异常告警行（degraded 快照标志） */
  degraded: boolean;
  /** 今日行：「今日 1.2M · 345 次调用」（无会话且零消耗时不显示） */
  today: string | null;
  /** 全部历史口径行（窄档隐藏的「累计」在此恒可查） */
  allTime: string | null;
  /** 额度段引导／不可用文案（未配置实例／查询暂不可用时非 null） */
  quotaNote: string | null;
  /** 各实例明细组（展示集为空时为数组空——整段不渲染，与纯文本版口径一致） */
  groups: TooltipGroup[];
}

/** 聚合状态显示名（原 IslandBar ISLAND_META 的 label 半边——dot 半边留渲染层；
 * quota_exhausted 按出错态口径，与胶囊红灯语义对齐） */
function islandStateLabel(snap: IslandSnapshot): string {
  const state = snap.quota_exhausted ? "any_error" : snap.island;
  switch (state) {
    case "no_sessions":
      return "未检测到会话";
    case "all_idle":
      return "空闲";
    case "any_working":
      return "有会话正在工作";
    case "any_waiting":
      return "有会话等待输入";
    case "any_error":
      return "有会话出错 / 额度耗尽";
  }
}

/** 窗口固定排序权重：5h → 7d → 其他（原 IslandBar windowRank 迁入，注释见彼处） */
function windowRank(kind: string): number {
  if (kind === "5h") return 0;
  if (kind === "weekly") return 1;
  return 2;
}

/** 单实例 → 结构化分组（纯文本版逐行拼装的结构化投影，文案逐字同源） */
function groupOf(
  a: IslandAccountView,
  thresholds: Thresholds,
): TooltipGroup {
  let balance: TooltipLine | null = null;
  if (a.balance) {
    // M3-7 按币种取警戒线：本币线拼装文案（无对应线不告急，不做凭空换算）
    const line = balanceWarnFor(thresholds, a.balance.currency);
    const low = line != null && a.balance.total < line;
    // 附注两段：警戒线说明（跌破才有）＋欠费说明（负余额才有）；全空为 null
    const note =
      (low ? `（低于警戒线 ${fmtMoney(a.balance.currency, line)}）` : "") +
      (a.balance.total < 0 ? "，账户欠费" : "");
    balance = {
      level: low ? "danger" : "normal",
      label: "余额",
      mono: fmtMoney(a.balance.currency, a.balance.total),
      post: note.length > 0 ? note : null,
    };
  }
  const quotas = [...a.quotas].sort(
    (x, y) => windowRank(x.window_kind) - windowRank(y.window_kind),
  );
  const rows: TooltipLine[] = quotas.map((q) => {
    const pct = q.used_percent ?? 0;
    const level = quotaLevel(pct, thresholds.warn, thresholds.danger);
    return {
      level,
      label: null,
      mono: `${windowLabel(q.window_kind)} ${Math.round(pct)}%`,
      post: q.reset_at != null ? resetPhrase(q.reset_at) : null,
    };
  });
  return {
    color: a.color,
    title: accountLabel(a.kind_name, a.alias),
    balance,
    rows,
    empty: !a.balance && a.quotas.length === 0 ? "暂无额度数据 · 约 5 分钟后自动重试" : null,
  };
}

/** 组装气泡载荷：snap 未就绪返回 null（悬停到期回调据此跳过 emit）。
 *  口径与被退役的 textTitle／quotaTitle／rootTitle 三段拼装完全一致 */
export function buildTooltipPayload(
  snap: IslandSnapshot | null,
  thresholds: Thresholds,
): TooltipPayload | null {
  if (!snap) return null;
  const label = islandStateLabel(snap);
  const endedCount = snap.sessions.filter((s) =>
    displayState(s.state, s.last_activity_at).ended,
  ).length;
  const status =
    snap.sessions.length > 0
      ? `${label} · 共 ${snap.sessions.length}（已结束 ${endedCount}）`
      : label;
  // 今日/累计行的显示门槛与岛胶囊同口径（有会话或今日有消耗才报数）
  const showTokens = snap.today_tokens > 0 || snap.sessions.length > 0;
  const allTime = snap.sessions.reduce((a, s) => a + s.session_tokens, 0);
  const islandAccounts = snap.accounts.filter((a) => a.in_island);
  // 额度段三态降级（与原 quotaTitle 同款）：未配置引导／展示集清空整段隐藏／
  // 选择器不可用提示；正常态产出全实例结构化分组
  let quotaNote: string | null = null;
  let groups: TooltipGroup[] = [];
  if (snap.accounts.length === 0) {
    quotaNote =
      "尚未配置供应商实例\n到设置页「额度与凭据」添加后可展示各厂商额度/余额";
  } else if (islandAccounts.length > 0) {
    if (!tensestAccount(islandAccounts, thresholds)) {
      quotaNote = "额度查询暂不可用，约 5 分钟后自动重试";
    } else {
      groups = islandAccounts.map((a) => groupOf(a, thresholds));
    }
  }
  return {
    status,
    degraded: snap.degraded ?? false,
    today: showTokens
      ? `今日 ${fmtTokens(snap.today_tokens)} · ${snap.today_calls.toLocaleString()} 次调用`
      : null,
    allTime: showTokens
      ? `全部历史 ${fmtTokens(allTime)} · 含缓存 token，与账单一致`
      : null,
    quotaNote,
    groups,
  };
}
