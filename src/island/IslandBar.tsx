/**
 * 收缩态胶囊（2026-09-28 视觉改版批次一重排）：状态灯 + 主文案 ｜分隔线｜ 额度仪表单元 + 今日。
 * 分组结构回应「元素漂在两端」：主文案区（灯+600 字重文案）与数据区（额度/今日）
 * 用细竖线分成两组，数据区内部走「标签灰＋数字 mono 加粗」的仪表语言；
 * 状态氛围光：aggregated 状态驱动整仓 4%~8% 着色（st-* 类，详见 App.css）；
 * 数字补间：百分比/今日 token 变化 300ms 滚动（useTween，reduced-motion 直落）。
 *
 * 历史行为保留：主文案"现在时"口径、额度最紧张实例选择（M3-6 归一）、
 * 整条可拖拽（位移阈值判别，见下方拖拽注释）。
 *
 * ⚠ 原生 title 全部聚合在根 div（2026-10-03 审查修复）：App.css
 * `.island > * { pointer-events: none }` 使子元素 title 永不触发，深化信息
 * （状态说明/会话对账/额度明细/今日口径）统一挂根节点单气泡。
 *
 * ⚠ 宽度降级阶梯（F7）：App.tsx 按岛宽注根类 w-lg/w-md/w-sm——
 * 窄档隐藏的「今日/厂商短名」信息由根 title 恒定承接，不丢口径
 */
import { useEffect, useRef, useState } from "react";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import type { IslandAccountView, IslandSnapshot, QuotaView, Thresholds } from "../shared/types";
import {
  accountLabel,
  balanceWarnFor,
  fmtTokens,
  quotaLevel,
  tensestAccount,
  windowLabel,
} from "../shared/types";
import { displayState } from "../shared/sessionDisplay";
import { useTween } from "../shared/useTween";
import { fmtMoney } from "../shared/format";
import { WarnIcon } from "../shared/icons";

/** 单击/拖拽判定的位移阈值（逻辑像素）：按下后移动超过该值才算拖拽 */
const DRAG_THRESHOLD_PX = 6;

/** 状态灯样式与聚合状态说明（title 用，主文案不再重复状态词——C1） */
const ISLAND_META: Record<
  IslandSnapshot["island"],
  { dot: string; label: string }
> = {
  no_sessions: { dot: "dot-gray", label: "未检测到会话" },
  all_idle: { dot: "dot-green", label: "空闲" },
  any_working: { dot: "dot-green breathe", label: "有会话正在工作" },
  any_waiting: { dot: "dot-amber", label: "有会话等待输入" },
  any_error: { dot: "dot-red pulse", label: "有会话出错 / 额度耗尽" },
};

/** 聚合状态 → 氛围光类（App.css .island.st-*）：空闲/无会话保持中性画布不着色 */
function ambientClass(islandState: IslandSnapshot["island"]): string {
  if (islandState === "any_working") return "st-working";
  if (islandState === "any_waiting") return "st-waiting";
  if (islandState === "any_error") return "st-error";
  return "";
}

/** 主文案（C2"现在时"方案）：只列非零的活跃计数，按出错 > 等待 > 工作中排序；
 *  额度耗尽并入文案（与红灯语义对齐）；状态词全行只出现一次（修 C1 重复）。
 *  F4 口径统一（批次三）：空闲计数只数未结束会话——与面板「会话 · 活跃/共」
 *  同一 displayState 判定，"13 个会话"里 10 个已结束的误读不再发生 */
function activeText(snap: IslandSnapshot): string {
  const count = (st: string) =>
    snap.sessions.filter((s) => s.state === st).length;
  const parts: string[] = [];
  const err = count("error");
  const waiting = count("waiting");
  const working = count("working");
  if (err > 0) parts.push(`${err} 出错`);
  if (waiting > 0) parts.push(`${waiting} 等输入`);
  if (working > 0) parts.push(`${working} 工作中`);
  // 额度耗尽但无会话出错：红灯由 quota_exhausted 驱动，文案同步说明，避免"红点+空闲"矛盾
  if (snap.quota_exhausted && err === 0) parts.push("额度耗尽");
  if (parts.length > 0) return parts.join(" · ");
  if (snap.sessions.length > 0) {
    const alive = snap.sessions.filter(
      (s) => !displayState(s.state, s.last_activity_at).ended,
    ).length;
    return alive > 0 ? `空闲 · ${alive} 个会话` : "空闲";
  }
  return "未检测到会话";
}

/** 紧凑倒计时（岛根 title 专用，2026-10-10 结构化重排）：零头归零＋「分钟」减为
 *  「分」——「6 天 0 小时」→「6 天」、「1 小时 40 分钟」→「1 小时 40 分」。
 *  共享 fmtCountdownCN 为面板/托盘/额度页同用，不动；此压缩只在纯文本行内做，
 *  避免共享函数变更被动波及他处口径。返回 null＝已过期待快照刷新（同族语义） */
function compactCountdown(resetAt: number): string | null {
  const diff = resetAt - Date.now();
  if (diff <= 0) return null;
  const m = Math.floor(diff / 60_000);
  const h = Math.floor(m / 60);
  const d = Math.floor(h / 24);
  const parts: string[] = [];
  if (d > 0) parts.push(`${d} 天`);
  if (h % 24 > 0) parts.push(`${h % 24} 小时`);
  if (m % 60 > 0) parts.push(`${m % 60} 分`);
  return parts.length > 0 ? parts.join(" ") : "不足 1 分钟";
}

/** 紧凑重置短语：「预计 X 后重置」→「X 后重置」，过期改说「等待供应商刷新」
 *  （与共享 resetPhrase 同一口径，仅去「预计」二字——行内已有窗口标与百分比，
 *  短语越短，N 实例重复模式的扫读负担越小） */
function resetPhraseCompact(resetAt: number): string {
  const cd = compactCountdown(resetAt);
  return cd != null ? `${cd}后重置` : "等待供应商刷新";
}

/** 窗口固定排序权重：5h → 7d → 其他（2026-10-10 拍板：不再跟随快照数组序——
 *  此前 KMDFB 5h 在前、KMDYG 却 7d 在前，无规律加重扫读混乱） */
function windowRank(kind: string): number {
  if (kind === "5h") return 0;
  if (kind === "weekly") return 1;
  return 2;
}

/** 额度段 tooltip（2026-10-10 结构化重排，所有者拍板）：纯文本 title 的结构手段
 *  只有换行/空行/缩进——实例间空行分组立段落、数据行全角空格缩进挂标题行下、
 *  ⚠ 前缀（quotaLevel warn 及以上）是唯一「变色」替代物，视线第一落点；
 *  5h/7d 去中文全称双写（全项目统一短标，逐行双写是字墙主因，一次性学习成本
 *  换永久减字）；余额实例带警戒线/欠费说明——数值口径与面板/托盘一致不变 */
function quotaOverviewTitle(
  accounts: IslandAccountView[],
  thresholds: Thresholds,
): string {
  return accounts
    .map((a) => {
      // 标题行独立成行，数据行统一全角空格缩进（标题＝组锚点，缩进＝隶属关系）
      const lines = [accountLabel(a.kind_name, a.alias)];
      if (a.balance) {
        // M3-7 按币种取警戒线：本币线拼装文案（无对应线不告急，不做凭空换算）
        const line = balanceWarnFor(thresholds, a.balance.currency);
        const low = line != null && a.balance.total < line;
        lines.push(
          `　余额 ${fmtMoney(a.balance.currency, a.balance.total)}${
            low ? `（低于警戒线 ${fmtMoney(a.balance.currency, line)}）` : ""
          }${a.balance.total < 0 ? "，账户欠费" : ""}`,
        );
      }
      const quotas = [...a.quotas].sort(
        (x, y) => windowRank(x.window_kind) - windowRank(y.window_kind),
      );
      for (const q of quotas) {
        const pct = q.used_percent ?? 0;
        // ⚠ 告警前缀：仅 warn/danger 档出现（纯文本里唯一的强调手段，克制使用）
        const flag =
          quotaLevel(pct, thresholds.warn, thresholds.danger) === "normal" ? "" : "⚠ ";
        // 过期（快照未刷新）改说「等待供应商刷新」（五轮审查病句修复口径保留）
        lines.push(
          `　${flag}${windowLabel(q.window_kind)} ${Math.round(pct)}%${
            q.reset_at != null ? ` · ${resetPhraseCompact(q.reset_at)}` : ""
          }`,
        );
      }
      if (!a.balance && a.quotas.length === 0)
        lines.push("　暂无额度数据 · 约 5 分钟后自动重试");
      return lines.join("\n");
    })
    .join("\n\n");
}

/** 窗口口径仪表单元（批次一）：厂商短名＋窗口短标＋迷你条＋补间百分比。
 *  迷你条让"健康度"在正常档也可见（长度通道），颜色只在告警档介入（Apple 色彩纪律）；
 *  百分比 300ms 补间滚动，快照 10s 一刷的跳变不再生硬 */
function QuotaGauge({
  account,
  quota,
  thresholds,
}: {
  account: IslandAccountView;
  quota: QuotaView;
  thresholds: Thresholds;
}) {
  const pct = Math.min(100, Math.max(0, quota.used_percent ?? 0));
  const shown = useTween(pct);
  const cls = quotaLevel(pct, thresholds.warn, thresholds.danger);
  return (
    <span
      className={`island-quota q-unit-win${cls !== "normal" ? ` quota-${cls}` : ""}`}
    >
      <span className="q-kind">{account.kind_name}</span>
      <span className="q-win">{windowLabel(quota.window_kind)}</span>
      <span className="q-track">
        <i
          className={`q-fill fill-${cls}`}
          style={{ transform: `scaleX(${shown / 100})` }}
        />
      </span>
      <b className="q-pct">{Math.round(shown)}%</b>
    </span>
  );
}

/** 余额口径单元（批次一）：厂商短名＋ mono 金额；跌破本币警戒线转 danger 红字。
 *  account 类型为 TensestPick 的 balance 分支收窄（balance 恒非空，编译器保证） */
function BalanceUnit({
  account,
  thresholds,
}: {
  account: IslandAccountView & { balance: NonNullable<IslandAccountView["balance"]> };
  thresholds: Thresholds;
}) {
  const b = account.balance;
  const line = balanceWarnFor(thresholds, b.currency);
  const low = line != null && b.total < line;
  return (
    <span
      className={`island-quota q-unit-bal${low ? " quota-danger" : ""}`}
    >
      <span className="q-kind">{account.kind_name}</span>
      <b className="q-pct">{fmtMoney(b.currency, b.total)}</b>
    </span>
  );
}

/** 收缩态额度段（M3-6 跨实例归一）：不轮播（48px 胶囊放不下指示器，点状
 *  轮播在展开面板），显示展示集内最紧张实例——余额跌破警戒线最优先（红字），
 *  否则窗口百分比最高；明细文案由胶囊根 title 聚合承载（见 IslandBar 末尾）。
 *  零实例＝灰字引导（C6 降级可见）；配置了但展示集被清空＝整体隐藏（尊重显式选择） */
function QuotaSegment({ snap, thresholds }: { snap: IslandSnapshot; thresholds: Thresholds }) {
  if (snap.accounts.length === 0) {
    return <span className="island-quota quota-off">额度未配置</span>;
  }
  const islandAccounts = snap.accounts.filter((a) => a.in_island);
  if (islandAccounts.length === 0) return null; // 展示集被清空：额度段整体隐藏
  const pick = tensestAccount(islandAccounts, thresholds);
  if (!pick) {
    return <span className="island-quota quota-off">额度 --</span>;
  }
  if (pick.reason === "balance") {
    return <BalanceUnit account={pick.account} thresholds={thresholds} />;
  }
  return <QuotaGauge account={pick.account} quota={pick.quota} thresholds={thresholds} />;
}

export default function IslandBar({
  snap,
  thresholds,
  enterEdge,
  onToggle,
}: {
  snap: IslandSnapshot | null;
  thresholds: Thresholds;
  /** 贴边隐藏态滑回显示时的贴边边（top/left/right）：触发"由远及近"入场动画，
   *  决定缩放 origin 与位移方向；null = 无动画（自由悬浮/启动直显） */
  enterEdge?: string | null;
  /** 悬停展开关闭时：点击胶囊切换信息卡片展开/收起 */
  onToggle?: () => void;
}) {
  // 额度耗尽时胶囊整体按出错态展示（会话状态本身保持真实值）
  const islandState = snap?.quota_exhausted ? "any_error" : snap?.island;
  const meta = snap ? ISLAND_META[islandState ?? "no_sessions"] : ISLAND_META.no_sessions;
  const ambient = snap ? ambientClass(islandState ?? "no_sessions") : "";

  // 今日消耗（C3）：账单口径（含缓存）；"累计"降级进 tooltip
  const allTime = snap?.sessions.reduce((a, s) => a + s.session_tokens, 0) ?? 0;
  const showTokens = snap != null && (snap.today_tokens > 0 || snap.sessions.length > 0);
  // 数字补间：快照 10s 一刷的 token 跳变改 300ms 滚动（reduced-motion 下直落）
  const todayShown = useTween(snap?.today_tokens ?? 0);

  // 事件粒子光环（批次三动效增强包①）：聚合状态「进入出错」的瞬间播一次
  // 边缘光环洗过动画（~0.9s 自熄）——粒子语言只做状态变化的标点，不做常驻装饰；
  // 首帧即出错（启动带病上线）不播，避免「开门就闪」
  const [ripple, setRipple] = useState(false);
  const prevErrorRef = useRef<boolean | null>(null);
  const isError = islandState === "any_error";
  useEffect(() => {
    const was = prevErrorRef.current;
    prevErrorRef.current = isError;
    if (was == null || was === isError) return;
    if (!isError) return;
    setRipple(true);
    const t = window.setTimeout(() => setRipple(false), 1000);
    return () => window.clearTimeout(t);
  }, [isError]);

  // 按压追踪：起点坐标 + 是否已进入系统拖拽（进入后松手不算单击）
  const pressOrigin = useRef<{ x: number; y: number } | null>(null);
  const dragging = useRef(false);

  /** 按下：仅记录起点，不立即拖拽（为单击判定留出位移余量） */
  const onMouseDown = (e: React.MouseEvent) => {
    if (e.button !== 0) return;
    pressOrigin.current = { x: e.clientX, y: e.clientY };
    dragging.current = false;
  };

  /** 按住移动：位移超阈值 → 交给系统拖拽（此后鼠标事件由系统接管，贴靠仍由 Rust 评估） */
  const onMouseMove = (e: React.MouseEvent) => {
    const o = pressOrigin.current;
    if (!o || dragging.current) return;
    if (Math.hypot(e.clientX - o.x, e.clientY - o.y) > DRAG_THRESHOLD_PX) {
      dragging.current = true;
      void getCurrentWebviewWindow().startDragging();
    }
  };

  /** 松手：未进入系统拖拽 = 单击 → 切换信息卡片（仅悬停展开关闭时 onToggle 有值） */
  const onMouseUp = () => {
    const wasDragging = dragging.current;
    pressOrigin.current = null;
    dragging.current = false;
    if (!wasDragging) onToggle?.();
  };

  // 主文案口径文案（并入根 title，2026-10-10 结构化重排）：状态词＋会话对账并一行
  // ——「有会话正在工作 · 共 138（已结束 137）」结论先行、对账免做减法（F4 口径不变）；
  // 今日口径随行携带——窄档（w-md/w-sm）隐藏「今日」 span 后仍可在此看到总数与调用次数（F7）
  const endedCount = snap
    ? snap.sessions.filter((s) => displayState(s.state, s.last_activity_at).ended).length
    : 0;
  const textTitle = snap
    ? [
        snap.sessions.length > 0
          ? `${meta.label} · 共 ${snap.sessions.length}（已结束 ${endedCount}）`
          : meta.label,
        snap.degraded ? "采集源连续失败，数据可能滞后" : "",
        showTokens ? `今日 ${fmtTokens(snap.today_tokens)} · ${snap.today_calls} 次调用` : "",
      ]
        .filter(Boolean)
        .join("\n")
    : "";

  // 额度段深化文案（并入根 title）：与 QuotaSegment 的降级分支同款口径
  const quotaTitle = (() => {
    if (!snap) return "";
    if (snap.accounts.length === 0)
      return "尚未配置供应商实例\n到设置页「额度与凭据」添加后可展示各厂商额度/余额";
    const islandAccounts = snap.accounts.filter((a) => a.in_island);
    if (islandAccounts.length === 0) return "";
    if (!tensestAccount(islandAccounts, thresholds)) return "额度查询暂不可用，约 5 分钟后自动重试";
    return quotaOverviewTitle(islandAccounts, thresholds);
  })();

  // 根 title 聚合（2026-10-03 审查修复）：App.css `.island > * { pointer-events: none }`
  // （整条可拖拽的 hit-test 需要）使全部子元素的原生 title 永不触发——深化信息
  // 统一聚合到根 div，悬停胶囊任意位置出一套完整气泡（窄档被隐信息恒可查）
  const rootTitle =
    [
      textTitle,
      quotaTitle,
      // 口径注并一行（2026-10-10 结构化重排）：低频信息不再独占两行
      showTokens && snap
        ? `全部历史 ${fmtTokens(allTime)} · 含缓存 token，与账单一致`
        : "",
    ]
      .filter(Boolean)
      // 段间空行（2026-10-10 结构化重排）：状态段｜各供应商组｜口径注三段立界——
      // 纯文本 title 里空行是最强分隔手段；段为空时 filter 已滤除，不会双空行
      .join("\n\n") || undefined;

  // 入场动画参数：origin 贴向停靠边、位移从边缘方向起始——顶部自上而下长出，
  // 左右贴边从屏幕边缘探出，配合 Rust 缓动滑入即"由小变大、由远及近"
  const enterStyle = enterEdge
    ? ({
        transformOrigin:
          enterEdge === "top"
            ? "50% 0%"
            : enterEdge === "left"
              ? "0% 50%"
              : "100% 50%",
        "--in-x": enterEdge === "top" ? "0px" : enterEdge === "left" ? "-10px" : "10px",
        "--in-y": enterEdge === "top" ? "-12px" : "0px",
      } as React.CSSProperties)
    : undefined;

  // 额度段渲染节点与「是否显示」判定：展示集被清空时 QuotaSegment 返回 null，
  // 分隔线随之隐藏（避免一组悬空的竖线）；其余情况（含未配置灰字）都伴分隔线。
  // 判定须与 QuotaSegment 同口径（五轮审查修复）：原先拿 quotaNode（JSX 元素，
  // 恒真值）判空——展示集被清空时竖线照渲染＝悬空竖线，与注释承诺相反
  const quotaNode = snap != null ? <QuotaSegment snap={snap} thresholds={thresholds} /> : null;
  const hasQuotaSeg =
    snap != null && (snap.accounts.length === 0 || snap.accounts.some((a) => a.in_island));

  return (
    <div
      className={`island${ambient ? ` ${ambient}` : ""}${onToggle ? " island-clickable" : ""}${enterEdge ? " island-peek-in" : ""}`}
      style={enterStyle}
      title={rootTitle}
      onMouseDown={onMouseDown}
      onMouseMove={onMouseMove}
      onMouseUp={onMouseUp}
      // 按下后未达拖拽阈值就移出胶囊松手：清除按压态，防止落点处的 mouseup 误判为单击
      onMouseLeave={() => {
        pressOrigin.current = null;
        dragging.current = false;
      }}
    >
      {/* 事件粒子光环：进入出错态的一次性边缘光环（自熄，reduced-motion 由全局收敛兜底） */}
      {ripple && <span className="island-halo" />}
      {/* 边缘微光呼吸（标配，2026-09-28 验收评审拍板删设置项）：强度刻意做淡——
          「注意不到它在呼吸，只觉得它是活的」；出错红边让位 */}
      <span className="island-glow" />
      <span className={`dot ${meta.dot}`} />
      <span className="island-text">
        {snap ? activeText(snap) : "「去你的岛」启动中…"}
        {
          // 降级可见（审查 1.1 + C7）：警示图标强化，与正常文案拉开视觉差
          snap?.degraded && (
            <span className="island-degraded">
              <WarnIcon /> 采集异常
            </span>
          )
        }
      </span>
      {hasQuotaSeg && <span className="island-sep" />}
      {
        // 额度段（M3-6 跨实例归一）：归一选择与三态降级收进 QuotaSegment
        quotaNode
      }
      {showTokens && (
        <span className="island-tokens">
          今日 <b>{fmtTokens(todayShown)}</b>
        </span>
      )}
    </div>
  );
}
