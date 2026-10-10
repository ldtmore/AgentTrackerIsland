/**
 * 额度页（M3-5，06-PLAN §8）：与报表并列的常规窗口（#quotas，开窗机制照抄
 * 报表页——常驻隐藏窗口只 show 不重建）。三段式骨架：
 * ① 汇总条：余额合计（Balance 口径实例按币种分组合计，仅启用实例计入）＋
 *    窗口口径实例计数＋数据截至＋手动刷新（重拉本地库，外呼不做在此处）；
 * ② 实例卡片流：复用设置页 ProviderCard（M3-5 插槽化——detail 承载完整
 *    额度体，actions 承载脚注）：Windows 口径＝每窗口进度条＋重置倒计时；
 *    Balance 口径＝金额大字＋赠金/现金拆分（M3-3 口径：available 三家统一
 *    无值，拆分行按 total−granted 反推现金）；LocalEstimate＝第五批预留。
 *    脚注＝上次刷新时间＋状态徽标＋单卡刷新（provider_account_refresh 真实
 *    落库，与设置页"检测"只查询不同；失败文案即时红字反馈＝network_error
 *    表达——自动查询的静默退避失败无持久化字段，靠时间戳陈旧性诚实呈现）；
 * ③ 厂商分组：>3 家按厂商折叠（默认展开可收起），≤3 家平铺。
 * 空态：引导直达设置页额度分区（goto-quota-section 事件，红线④零阻塞）。
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import Tip from "../shared/Tip";
import EmptyState from "../shared/EmptyState";
import Toast, { type ToastData } from "../shared/Toast";
import { ChevronIcon, CoinIcon, InfoIcon, WarnIcon } from "../shared/icons";
import { invokeReady } from "../shared/invokeReady";
import ProviderCard, { fetchedAtOf } from "../shared/ProviderCard";
import {
  refreshAccount,
  type AccountOverview,
  type ProviderKindView,
} from "../shared/providerTypes";
import { useTheme } from "../shared/theme";
import {
  balanceWarnFor,
  DEFAULT_THRESHOLDS,
  accountLabel,
  fmtCountdownCN,
  resetPhrase,
  quotaLevel,
  sanitizeThresholds,
  windowLabel,
  windowLabelCN,
  type Thresholds,
} from "../shared/types";
import { fmtDT, fmtDTFull, currencySymbol } from "../shared/format";
import "./quotas.css";

/** 折叠分组记忆键（sessionStorage，会话级——窗口常驻隐藏形态下保持同一次
 *  使用内的收起状态，不跨会话持久化） */
const COLLAPSED_KEY = "qt-collapsed-groups";

/** 刷新失败的展示层归译（C1 审查修正）：鉴权类英文原文（如"HTTP 401
 *  Unauthorized"）对用户不友好，归译为动作导向文案。仅当原文不含中文指引词
 *  时才归译——Moonshot 等适配器的 401 自带"（注意：国内外站 key 混用）"
 *  详细提示，不能覆盖；其余错误保留原文（排障需要） */
function humanizeRefreshError(e: unknown): string {
  const s = String(e);
  const guided = /请|注意|建议|检查/.test(s);
  if (!guided && (/HTTP 401/i.test(s) || /unauthorized/i.test(s))) {
    return "鉴权失败（HTTP 401），请核对 Key 或接口地址";
  }
  if (!guided && (/HTTP 403/i.test(s) || /forbidden/i.test(s))) {
    return "访问被拒（HTTP 403），请确认该 Key 有余额查询权限";
  }
  return s;
}

/** 实例的口径判定（注册表缺该厂商时按快照数据兜底） */
function quotaKindOf(a: AccountOverview, kinds: Map<string, ProviderKindView>): string {
  return kinds.get(a.kind_id)?.quota_kind ?? (a.balance ? "balance" : "windows");
}

/** Windows 口径的窗口条组（E1 审查修正）：条行只保留三列（短标签＋条＋
 *  已用%，条为弹性满宽），重置倒计时移到条下方独立小字行——此前倒计时与
 *  百分比同行，行尾宽度不同把条挤压得长短不一，视觉上像用量比例差异；
 *  现在同卡所有条永远等长。展示标签用 windowLabel 短写（5h/7d），
 *  悬浮给中文全称与完整语义 */
function QuotaWindowRow({
  accountId,
  windowKind,
  usedPercent,
  resetAt,
  thresholds,
}: {
  accountId: string;
  windowKind: string;
  usedPercent: number | null;
  resetAt: number | null;
  thresholds: Thresholds;
}) {
  const pct = usedPercent != null ? Math.min(100, Math.max(0, usedPercent)) : null;
  const level = pct != null ? quotaLevel(pct, thresholds.warn, thresholds.danger) : "normal";
  // pace 燃烧速度预警（2026-09-29 审查新增 #21）：近 2h 采样斜率外推"重置时刻
  // 的预测值"——到限前主动预警（对标 QuotaBar 的 pace warning）。只在高危时
  // 显示（≥85% 琥珀／将用完红色），常态保持安静（宁缺毋滥的告警纪律）
  const [paceText, setPaceText] = useState<{ text: string; bad: boolean } | null>(null);
  // 窗口重置反馈动效（#26）：reset_at 跳到更晚的新值且旧值已到期＝窗口刚重置，
  // 进度条播一次"满格扫过回落"动画（对标 QuotaBar 的 ring sweep；一次性自熄）
  const prevResetRef = useRef<number | null>(null);
  const [justReset, setJustReset] = useState(false);
  useEffect(() => {
    const prev = prevResetRef.current;
    if (resetAt != null && prev != null && resetAt > prev && prev <= Date.now()) {
      setJustReset(true);
      const t = setTimeout(() => setJustReset(false), 1000);
      prevResetRef.current = resetAt;
      return () => clearTimeout(t);
    }
    prevResetRef.current = resetAt;
  }, [resetAt]);
  useEffect(() => {
    let alive = true;
    invoke<{ rate_per_hour: number; projected_at_reset: number | null; minutes_to_exhaust: number | null } | null>(
      "quota_pace",
      { accountId, windowKind },
    )
      .then((p) => {
        if (!alive || !p?.projected_at_reset) return;
        const proj = p.projected_at_reset;
        if (proj >= 100) {
          setPaceText({
            text: `按近 2h 速度重置前将用完（${p.minutes_to_exhaust != null ? `约 ${Math.max(1, Math.round(p.minutes_to_exhaust))} 分钟后到限` : "已接近用尽"}）`,
            bad: true,
          });
        } else if (proj >= 85) {
          setPaceText({ text: `按近 2h 速度，重置前预计到 ${Math.round(proj)}%`, bad: false });
        }
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [accountId, windowKind, usedPercent, resetAt]);
  // 悬浮语义与岛面板 QuotaChip 同款单行无歧义文案
  let status: string;
  if (pct == null) {
    status = "暂无数据，约 5 分钟后自动重试";
  } else if (resetAt == null) {
    status = pct === 0 ? "窗口刚刷新，暂无用量" : `已用 ${Math.round(pct)}% · 重置时间待供应商返回`;
  } else {
    status = `已用 ${Math.round(pct)}% · ${resetPhrase(resetAt)}`;
  }
  // 倒计时小字（D1）：100% 用尽时前缀"等待"，与常规爬升态区分开重点；
  // 过期（快照未刷新）改说「等待供应商刷新」，不拼病句（五轮审查）
  const resetText =
    resetAt != null
      ? (() => {
          const cd = fmtCountdownCN(resetAt);
          if (cd == null) return "等待供应商刷新";
          return pct != null && pct >= 100 ? `等待 ${cd}后重置` : `${cd}后重置`;
        })()
      : null;
  return (
    <div className="qt-qwingroup" title={`${windowLabelCN(windowKind)}额度：${status}`}>
      <div className="qt-qwin">
        <span className="qt-qwin-label">{windowLabel(windowKind)}</span>
        <div className="qt-qbar">
          <div
            className={`qt-qbar-fill fill-${level}${justReset ? " qt-just-reset" : ""}`}
            style={{ transform: `scaleX(${(pct ?? 0) / 100})` }}
          />
        </div>
        <span className={`qt-qwin-pct text-${level}`}>
          {pct != null ? `${Math.round(pct)}%` : "--"}
        </span>
      </div>
      {resetText && <div className="qt-qwin-reset">{resetText}</div>}
      {paceText && (
        <div className={`qt-qwin-pace${paceText.bad ? " qt-pace-bad" : ""}`}>{paceText.text}</div>
      )}
    </div>
  );
}

/** 重置时间表（#22，2026-09-29 布局优化改内联段）：启用实例当前快照的
 *  reset_at 按自然日分组——5h 窗口的重置全在"今天"、只有周窗口才有跨天事件，
 *  标题因此叫「即将重置」而非"未来 7 天"（名不副实）。作为总览卡的下段
 *  内联渲染（自带细分隔线），无事件返回 null 整段隐藏 */
function ResetCalendar({
  accounts,
}: {
  accounts: AccountOverview[];
}) {
  // 收集启用实例窗口快照里未来 7 天内的重置事件
  const now = Date.now();
  const events: { at: number; label: string; win: string }[] = [];
  for (const a of accounts) {
    for (const q of a.quotas) {
      if (q.reset_at != null && q.reset_at > now && q.reset_at < now + 7 * 86_400_000) {
        events.push({
          at: q.reset_at,
          // 2026-10-10：kind_name 走后端双站变体投影，accountLabel 消除同名重复
          label: accountLabel(a.kind_name, a.alias),
          win: windowLabel(q.window_kind),
        });
      }
    }
  }
  events.sort((x, y) => x.at - y.at);
  if (events.length === 0) return null;
  // 事件 chip（2026-09-29 左右布局改版）：日名并入 chip 文案（"今天 14:00 …"），
  // 单行横排 wrap——比逐日表式行省一半以上高度，总览卡不再挤压下方卡片流
  const p2 = (n: number) => String(n).padStart(2, "0");
  const dayShort = (t: number) => {
    const d = new Date(t);
    const today = new Date();
    const sameDay = (a: Date, b: Date) =>
      `${a.getFullYear()}-${p2(a.getMonth() + 1)}-${p2(a.getDate())}` ===
      `${b.getFullYear()}-${p2(b.getMonth() + 1)}-${p2(b.getDate())}`;
    if (sameDay(d, today)) return "今天";
    if (sameDay(d, new Date(today.getTime() + 86_400_000))) return "明天";
    return `${p2(d.getMonth() + 1)}-${p2(d.getDate())}`;
  };
  return (
    <>
      <div className="qt-overview-sep" />
      <div className="qt-cal">
        <span className="qt-cal-title">即将重置</span>
        <div className="qt-cal-items">
          {events.map((ev, i) => (
            <span key={i} className="qt-cal-item">
              {dayShort(ev.at)}{" "}
              {new Date(ev.at).getHours().toString().padStart(2, "0")}:
              {new Date(ev.at).getMinutes().toString().padStart(2, "0")}{" "}
              {ev.label}（{ev.win}）
            </span>
          ))}
        </div>
      </div>
    </>
  );
}

/** 额度体（ProviderCard detail 插槽）：按实例口径分型渲染（06 §8.2 三口径） */
function QuotaDetail({
  a,
  kinds,
  thresholds,
}: {
  a: AccountOverview;
  kinds: Map<string, ProviderKindView>;
  thresholds: Thresholds;
}) {
  // 凭据失效 → 历史小标（A2 审查修正）：当前展示为最近一次成功查询的数据
  // 而非实时值——三个口径分支都要承载（此前仅 Balance 分支有，Windows 卡
  // 凭据丢失＋旧数据红条会被误读为"实时用尽"）
  const staleBadge = a.cred_error ? (
    <span className="pc-stale" title="凭据失效，以下为最近一次成功查询的数据">
      历史
    </span>
  ) : null;
  const qk = quotaKindOf(a, kinds);
  // Balance 口径：金额大字 + 赠金/现金拆分（负数=欠费、低于警戒线都标红）
  if (qk === "balance") {
    if (!a.balance) {
      return (
        <div className="qt-qempty">
          暂无余额数据，自动查询约 5 分钟一轮；也可点下方「查询」立即获取
        </div>
      );
    }
    const b = a.balance;
    const neg = b.total < 0;
    // M3-7 按币种取警戒线：无对应线的币种不标红（不做凭空换算）
    const line = balanceWarnFor(thresholds, b.currency) ?? 0;
    const low = !neg && line > 0 && b.total < line;
    const sym = currencySymbol(b.currency);
    return (
      <div>
        <div className="qt-amount-line">
          <span
            className={`qt-amount${neg || low ? " qt-amount-bad" : ""}`}
            title={`余额快照 · 最近数据 ${fmtDT(b.fetched_at)}${low ? "（已跌破警戒线）" : ""}`}
          >
            {sym}
            {b.total.toFixed(2)}
          </span>
          {staleBadge}
          {neg && <span className="qt-amount-tag">欠费</span>}
          {low && <span className="qt-amount-tag">低于警戒线</span>}
        </div>
        {/* 拆分行：granted=赠金，total−granted=现金（充值）；无赠金数据不拆分 */}
        {b.granted != null && (
          <div className="qt-amount-split">
            赠金 {sym}
            {b.granted.toFixed(2)} · 现金 {sym}
            {(b.total - b.granted).toFixed(2)}
          </div>
        )}
      </div>
    );
  }
  // Windows／LocalEstimate 口径共用窗口条渲染（M3-12：推算值徽标＋说明；
  // 7d 行 used_percent 为 null＝限额未公开，QuotaWindowRow 显"--"）
  if (a.quotas.length === 0) {
    return (
      <div className="qt-qempty">
        暂无额度数据，自动查询约 5 分钟一轮；也可点下方「查询」立即获取
      </div>
    );
  }
  return (
    <div className="qt-qwins">
      {qk === "local_estimate" && (
        <div className="qt-stale-row">
          <span
            className="pc-stale"
            /* 顺手修（07-UX 3.9 批次内发现）：原为 JSX 字符串属性 title="…\n…"——
               属性串里的 \n 是字面反斜杠＋n 不换行，悬浮显示成带「\n」字样的
               一行长串；改表达式形式走 JS 字符串才是真换行（悬浮提示优化批次
               「原生 title 天然支持 \n」的立意即此） */
            title={"5h 限额为社区测算值\n仅统计 Claude Code 用量\n7 天合计无限额仅展示"}
          >
            推算值
          </span>
        </div>
      )}
      {staleBadge && <div className="qt-stale-row">{staleBadge}</div>}
      {a.quotas.map((q) => (
        <QuotaWindowRow
          key={q.window_kind}
          accountId={a.id}
          windowKind={q.window_kind}
          usedPercent={q.used_percent}
          resetAt={q.reset_at}
          thresholds={thresholds}
        />
      ))}
    </div>
  );
}

/** 单实例额度卡：ProviderCard 插槽化用法（纯查看——无开关/检测/编辑/删除），
 *  脚注 = 上次刷新时间 + 状态徽标（异常态才出现，正常态保持安静）+ 单卡刷新 */
function QuotaCard({
  a,
  kind,
  kinds,
  thresholds,
  onChanged,
}: {
  a: AccountOverview;
  kind?: ProviderKindView;
  kinds: Map<string, ProviderKindView>;
  thresholds: Thresholds;
  /** 刷新成功后回调（页面重拉 overview） */
  onChanged: () => Promise<void>;
}) {
  const [refreshing, setRefreshing] = useState(false);
  const [refreshErr, setRefreshErr] = useState<string | null>(null);
  const doRefresh = async () => {
    setRefreshing(true);
    setRefreshErr(null);
    try {
      await refreshAccount(a.id); // 真实落库（与检测只查询不同）
      await onChanged();
    } catch (e) {
      // 失败文案即时显示在该卡上＝network_error 反馈；401/403 归译为
      // 动作导向文案（C1，自带中文指引的适配器文案原样保留）
      setRefreshErr(humanizeRefreshError(e));
    } finally {
      setRefreshing(false);
    }
  };

  // 状态徽标（06 §4.2 五态按现有持久数据落地；B1 审查去重）：已停用（灰）→
  // 从未刷新（琥珀）→ 正常（无徽标保持安静）。凭据异常不再出脚注徽标——
  // 卡上红字全文已承载，同卡重复一遍是视觉噪声（同 M3-4 同文去重逻辑）；
  // credential_expired 属四批逆向 OAuth 类，当前四家不产生，无需枚举
  let badge: React.ReactNode = null;
  if (!a.enabled) {
    badge = <span className="qt-badge qt-badge-off">已停用</span>;
  } else if (!a.cred_error && a.quotas.length === 0 && !a.balance) {
    badge = <span className="qt-badge qt-badge-warn">从未刷新</span>;
  }

  const hasSnapshot = a.quotas.length > 0 || a.balance != null;
  return (
    <ProviderCard
      account={a}
      kind={kind}
      thresholds={thresholds}
      hideSwitches
      onCredAction={goSettings} // A3：红字行尾「去设置页处理」，补齐额度页动作出口
      detail={
        <QuotaDetail a={a} kinds={kinds} thresholds={thresholds} />
      }
      actions={
        <>
            {/* 查询失败全文悬浮兜底（C5）：错误文案超长会 ellipsis 截断——
                截断的错误信息最伤排障，title 恒可读全文 */}
            {refreshErr && (
              <div className="qt-refresh-err" title={refreshErr}>
                ✗ {refreshErr}
              </div>
            )}
          <div className="qt-foot">
            {/* B2：无快照时时间位留空——"从未刷新"徽标已独立承载，两处同义是重复 */}
            <span
              className="qt-foot-time"
              title={hasSnapshot ? "最近一次成功查询时间" : undefined}
            >
              {hasSnapshot ? fmtDT(fetchedAtOf(a)) : ""}
            </span>
            {badge}
            <button
              type="button"
              className="qt-refresh"
              disabled={refreshing || !a.enabled}
              title={
                a.enabled
                  ? "立即查询该实例额度（真实落库）\n与页头「刷新」重读本地库不同"
                  : "实例已停用，启用后才能查询"
              }
              onClick={doRefresh}
            >
              {/* C2：外呼语义用「查询」，与页头「刷新」（重读本地库）区分 */}
              {refreshing ? "查询中…" : "查询"}
            </button>
          </div>
        </>
      }
    />
  );
}

/** 打开设置窗口并滚动定位到「额度与凭据」分区（空态引导与凭据红字的
 *  「去设置页处理」出口共用；设置窗口常驻隐藏，监听器一直存活） */
function goSettings() {
  invoke("show_settings_window").catch(() => {});
  emit("goto-quota-section").catch(() => {});
}

export default function Quotas() {
  useTheme({ syncNative: true }); // 深浅主题跟随；syncNative＝标题栏颜色随应用主题（07-UX 2.4）
  const [kinds, setKinds] = useState<ProviderKindView[]>([]);
  const [accounts, setAccounts] = useState<AccountOverview[]>([]);
  const [loaded, setLoaded] = useState(false);
  // 首次加载错误（2026-10-03 审查修复：与零实例空态区分的显式错误态）
  const [loadError, setLoadError] = useState<string | null>(null);
  // 阈值水位（窗口百分比分色）与余额警戒线（金额标红，按币种分设），与岛/托盘同源
  const [thresholds, setThresholds] = useState<Thresholds>(DEFAULT_THRESHOLDS);
  // 「数据截至」（本次查询本地库时点，与报表页同一诚实模式）＋手动刷新触发器
  const [stamp, setStamp] = useState<string | null>(null);
  const [refreshTick, setRefreshTick] = useState(0);
  // 刷新失败 toast（2026-10-10 审查收口）：手动刷新/实例变更重拉失败显式报告，
  // 错误常驻可关闭（与会话页/报表页共享 Toast 语义）
  const [toast, setToast] = useState<ToastData | null>(null);
  // 厂商折叠组（>3 家启用）：收起的厂商 kind_id 集合。sessionStorage 记忆
  //（2026-09-29 布局优化：原纯会话级 state，窗口隐藏再打开回全展开——窗口是
  //  常驻隐藏形态，"同一次使用"内的收起状态应保持；不跨会话持久化）
  const [collapsed, setCollapsed] = useState<Set<string>>(() => {
    try {
      const raw = sessionStorage.getItem(COLLAPSED_KEY);
      return raw ? new Set(JSON.parse(raw) as string[]) : new Set();
    } catch {
      return new Set();
    }
  });
  // 深链定位的实例 id（2026-09-29 审查修复 #12：托盘明细行点击直达该实例卡片，
  // 而非只打开额度页；定位后短暂高亮，数秒自动消隐）
  const [focusId, setFocusId] = useState<string | null>(null);

  /** 重拉实例列表（手动刷新与单卡刷新共用；本地毫秒级）；成功即清错误态 */
  const reloadAccounts = async () => {
    const accs = await invokeReady<AccountOverview[]>("provider_overview");
    setAccounts(accs);
    setLoadError(null);
    setStamp(fmtDTFull(Date.now()));
  };

  // 首次加载：实例＋厂商清单＋阈值设置。走 invokeReady 重试——本窗口随应用
  // 启动即创建加载，挂载可能早于后端 setup 完成（HANDOFF 踩坑：state not managed）
  useEffect(() => {
    let alive = true;
    (async () => {
      try {
        const [ks, , s] = await Promise.all([
          invokeReady<ProviderKindView[]>("provider_kinds"),
          reloadAccounts(),
          invokeReady<Record<string, string>>("get_settings"),
        ]);
        if (!alive) return;
        setKinds(ks);
        // 阈值清洗收敛 shared/types（四轮审查：原三处各写一份，此处还与岛端
        // 略有口径差——warn/danger 非法时岛回退默认、此处保留旧值，现统一走
        // 岛端口径；余额警戒线 M3-7 按币种分设仍并入 thresholds 单一来源）
        setThresholds(sanitizeThresholds(s));
      } catch (e) {
        // 加载失败显式错误态（2026-10-03 审查修复）：原先吞错按空态展示——
        // 后端未就绪/查询故障会被误导为「还没有供应商」，把用户引向设置页
        // 折腾添加；与空态区分，提供重试出口
        if (alive) setLoadError(String(e));
      } finally {
        if (alive) setLoaded(true);
      }
    })();
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 手动刷新（refreshTick 由按钮触发；首次加载由上方 mount effect 承担）
  useEffect(() => {
    if (refreshTick === 0) return;
    // 失败反馈收口（2026-10-10 审查）：原仅 setStamp(null) 静默——唯一变化是
    // 「数据截至」角标消失极难察觉；改错误 toast 显式报告。旧列表保留、角标
    // 维持上次成功时点（本就是更诚实的「数据截至」语义，不再主动抹掉）
    reloadAccounts().catch((e) => {
      setToast({ text: `刷新失败：${String(e)}`, kind: "error" });
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [refreshTick]);

  // 实例表变更推送（E2 审查修正）：设置页增删改/启停/展示集切换后即时重拉，
  // 不再依赖手动刷新才看到新实例。与 agents-changed 同款模式；
  // 5 分钟一轮的自动查询落库不推事件，仍靠「数据截至」诚实呈现
  useEffect(() => {
    const un = listen("provider-accounts-changed", () => {
      // 即时重拉失败同样显式报告（2026-10-10 审查：原静默吞掉，用户只看到旧列表）
      reloadAccounts().catch((e) => {
        setToast({ text: `实例列表刷新失败：${String(e)}`, kind: "error" });
      });
    });
    return () => {
      un.then((f) => f());
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 托盘深链（#12）：goto-quota-account 携带实例 id——展开所属厂商分组＋滚动
  // 定位＋高亮 2.4s。事件由托盘明细行先 emit 再开窗（本窗口常驻已挂载监听）
  useEffect(() => {
    const un = listen<{ accountId: string }>("goto-quota-account", (ev) => {
      const id = ev.payload?.accountId;
      if (!id) return;
      const acc = accounts.find((a) => a.id === id);
      if (acc) {
        // 厂商分组若收起先展开（否则卡片不在 DOM，无法定位）
        setCollapsed((prev) => {
          if (!prev.has(acc.kind_id)) return prev;
          const next = new Set(prev);
          next.delete(acc.kind_id);
          return next;
        });
      }
      setFocusId(id);
      // 等分组展开渲染完成后再滚动（常驻隐藏窗口 rAF 挂起，用宏任务兜底一拍）；
      // 高亮 2.4s 自清（组件卸载后 setState 为 no-op，无需额外清理）。
      // 滚动尊重系统减弱动效（2026-10-03 审查补充，对齐设置页 scrollToSection 判定）
      const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
      setTimeout(() => {
        document
          .getElementById(`qt-card-${id}`)
          ?.scrollIntoView({ behavior: reduced ? "auto" : "smooth", block: "center" });
      }, 60);
      setTimeout(() => setFocusId((cur) => (cur === id ? null : cur)), 2400);
    });
    return () => {
      un.then((f) => f());
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [accounts]);

  const kindsMap = useMemo(
    () => new Map(kinds.map((k) => [k.id, k])),
    [kinds],
  );

  // 厂商分组（保持实例表创建顺序）：>3 家折叠分组，≤3 家平铺（06 §8.2）
  const groups = useMemo(() => {
    const map = new Map<string, AccountOverview[]>();
    for (const a of accounts) {
      const list = map.get(a.kind_id);
      if (list) list.push(a);
      else map.set(a.kind_id, [a]);
    }
    return [...map.entries()];
  }, [accounts]);
  const useGroups = groups.length > 3;
  const toggleGroup = (kid: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(kid)) next.delete(kid);
      else next.add(kid);
      // 同步写 sessionStorage（写入失败不影响交互）
      try {
        sessionStorage.setItem(COLLAPSED_KEY, JSON.stringify([...next]));
      } catch {
        /* 隐私模式等场景禁写：退化为纯内存态 */
      }
      return next;
    });

  // 汇总条口径：仅启用实例计入——停用实例的快照是旧数据，合计会失真
  const enabled = accounts.filter((a) => a.enabled);
  const balSums = new Map<string, number>();
  for (const a of enabled) {
    if (a.balance) {
      balSums.set(
        a.balance.currency,
        (balSums.get(a.balance.currency) ?? 0) + a.balance.total,
      );
    }
  }
  const balancePart = [...balSums.entries()];
  const windowsCount = enabled.filter((a) => quotaKindOf(a, kindsMap) === "windows").length;
  // B3 审查修正：双计数——Balance 实例即使暂无余额数据也计数，
  // 汇总条口径与卡片数对得上（此前只数窗口口径，4 张卡写 3 个让人对不上）
  const balanceCount = enabled.filter((a) => quotaKindOf(a, kindsMap) === "balance").length;
  // 推算口径计数（2026-10-03 审查修复）：local_estimate 是第三口径（Claude 订阅
  // 推算等），原先漏计——只建推算实例时卡片流有卡、汇总条却说「暂无额度数据」
  const localCount = enabled.filter((a) => quotaKindOf(a, kindsMap) === "local_estimate").length;
  // 汇总口径提示（语义分行：余额口径／计数口径／明细指引各占一行，
  // pre-line 渲染 \n——原先分号串联一行难扫读）。07-UX 3.9：分隔符改「·」
  // 后措辞同步点明「分隔非求和」，消跨币种相加歧义
  const sumTip =
    "余额为启用实例最新快照的合计，按币种分列（· 为分隔符，非跨币种求和）\n实例计数按口径分类（仅启用实例），停用实例不计入\n单实例口径见各卡片";

  const renderCard = (list: AccountOverview[]) =>
    list.map((a) => (
      <div
        key={a.id}
        id={`qt-card-${a.id}`}
        className={focusId === a.id ? "qt-card-focus" : undefined}
      >
        <QuotaCard
          a={a}
          kind={kindsMap.get(a.kind_id)}
          kinds={kindsMap}
          thresholds={thresholds}
          onChanged={reloadAccounts}
        />
      </div>
    ));

  return (
    <div className="qt-root">
      <div className="qt-header">
        <span className="qt-title">额度</span>
        <div className="qt-header-right">
          {stamp && (
            <Tip
              /* 引号属性里的 \n 是字面反斜杠＋n，改表达式形式走 JS 字符串才是真换行
                 （同上「推算值」title 修复，2026-10-10 全局清残余） */
              content={"打开/刷新时查询本地数据库的时间点（不做自动轮询）\n外呼查询见各卡片刷新按钮"}
            >
              <span className="qt-stamp">数据截至 {stamp.slice(5)}</span>
            </Tip>
          )}
          <button className="qt-btn" onClick={() => setRefreshTick((t) => t + 1)}>
            刷新
          </button>
          {/* 设置入口（07-UX 1.3 统一形态）：与报表/会话页头部同文案文字按钮，
              「直达设置页『额度与凭据』分区」的定向行为保留（goSettings 内事件）；
              置于末端与两页「设置恒在最右」同规则（原「管理」的「低频在前」
              排序理由随形态统一失效），定向说明由 title 承载 */}
          <button
            className="qt-btn"
            title="打开设置页的「额度与凭据」分区，管理供应商实例"
            onClick={goSettings}
          >
            设置
          </button>
        </div>
      </div>

      {/* 错误态（2026-10-03 审查修复）：加载失败与零实例空态区分——失败不是
          「还没有供应商」，给重试出口而不是引导去设置页折腾添加；
          07-UX 2.1 起统一走 EmptyState 模板（WarnIcon 区分排障方向） */}
      {loadError ? (
        <EmptyState
          icon={<WarnIcon />}
          title="加载失败"
          desc={loadError}
          action={{
            label: "重试",
            onClick: () => {
              setLoadError(null);
              setRefreshTick((t) => t + 1);
            },
          }}
        />
      ) : loaded && accounts.length === 0 ? (
        // 真空态（07-UX 2.1 统一模板）：原 .qt-empty 结构即本模板蓝本，改用
        // 共享组件后三页（报表/会话/额度）真空态同构
        <EmptyState
          icon={<CoinIcon />}
          title="还没有供应商"
          desc="添加 AI 供应商实例后，这里会展示各家的余额与窗口用量"
          action={{ label: "去设置添加", onClick: goSettings }}
        />
      ) : (
        <>
          {/* 总览卡（2026-09-29 布局优化）：原汇总条与重置日历两个孤块合并为
              单卡两段——上段余额合计与实例计数、细分隔线、下段即将重置；
              下段无事件自然退化（分隔线随内联段一起隐藏），页面回归
              「头部→一张总览卡→卡片流」的一总一分结构，卡片语言唯一 */}
          <div className="qt-overview">
            <div className="qt-sumbar">
              {balancePart.length > 0 ? (
                <>
                  <span className="qt-sum-label">余额合计</span>
                  {balancePart.map(([cur, sum], i) => (
                    <span key={cur} className="qt-sum-num" title="启用实例最新余额快照合计">
                      {/* 分隔符「·」（07-UX 3.9）：原「＋」有跨币种求和歧义 */}
                      {i > 0 && <span className="qt-sum-sep"> · </span>}
                      {currencySymbol(cur)}
                      {sum.toFixed(2)}
                    </span>
                  ))}
                </>
              ) : null}
              {windowsCount > 0 && (
                <span className="qt-sum-sub">
                  窗口额度 <b>{windowsCount}</b> 个实例
                </span>
              )}
              {balanceCount > 0 && (
                <span className="qt-sum-sub">
                  余额 <b>{balanceCount}</b> 个实例
                </span>
              )}
              {localCount > 0 && (
                <span className="qt-sum-sub">
                  推算 <b>{localCount}</b> 个实例
                </span>
              )}
              {windowsCount === 0 && balanceCount === 0 && localCount === 0 && (
                <span className="qt-sum-none">
                  {loaded ? "暂无额度数据（仅统计启用实例）" : "加载中…"}
                </span>
              )}
              <Tip content={sumTip}>
                <span className="qt-sum-label qt-sum-tip"><InfoIcon /> 口径</span>
              </Tip>
            </div>

            {/* 重置时间表（#22）：总览卡下段内联（自带分隔线；无事件整段隐藏） */}
            <ResetCalendar accounts={enabled} />
          </div>

          {/* 实例卡片流：≤3 家平铺；>3 家按厂商折叠分组（默认展开） */}
          {useGroups
            ? groups.map(([kid, list]) => {
                const k = kindsMap.get(kid);
                const open = !collapsed.has(kid);
                return (
                  <div className="qt-group" key={kid}>
                    <button
                      type="button"
                      className="qt-group-head"
                      onClick={() => toggleGroup(kid)}
                      title={open ? "点击收起该供应商" : "点击展开该供应商"}
                    >
                      <span className="qt-group-chevron"><ChevronIcon dir={open ? "down" : "right"} /></span>
                      <i className="pc-dot" style={{ background: k?.color ?? "#8a8f98" }} />
                      <span>{k?.name ?? kid}</span>
                      <span className="qt-group-count">{list.length} 个实例</span>
                    </button>
                    {open && <div className="qt-cards">{renderCard(list)}</div>}
                  </div>
                );
              })
            : <div className="qt-cards">{groups.map(([, list]) => renderCard(list))}</div>}
        </>
      )}
      {/* 刷新失败 toast（2026-10-10 审查）：fixed 贴窗顶居中，与会话页/报表页同款 */}
      <Toast data={toast} onDismiss={() => setToast(null)} />
    </div>
  );
}
