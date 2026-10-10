/**
 * 托盘菜单页（#tray-menu）：webview 自绘托盘菜单（M1-9）。
 * 原生菜单（muda → Win32）无样式 API，托盘右键改为弹出本窗口：
 * 行高/间距/圆角/动效/双主题全部可控，样式只引用 App.css 的岛面板变量体系。
 * 数据零新增查询——信息头订阅与岛窗口同一份 island-snapshot 广播（聚合器 10s 一拍）；
 * 动作经 tray_menu_action 复用托盘旧有逻辑（toggle_island/show_aux_window/退出）。
 * 收起路径：失焦（Rust 侧 Focused 事件）、Esc、点击菜单项后自隐。
 */
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { emit, listen } from "@tauri-apps/api/event";
import { useTheme } from "../shared/theme";
import {
  accountLabel,
  fmtCountdownCN,
  fmtTokens,
  balanceWarnFor,
  DEFAULT_THRESHOLDS,
  quotaLevel,
  sanitizeThresholds,
  SESSION_META,
  tensestAccount,
  windowLabel,
  type IslandAccountView,
  type IslandSnapshot,
  type SessionState,
  type Thresholds,
} from "../shared/types";
import { fmtDT, fmtMoney } from "../shared/format";
import { CoinIcon, GearIcon, InfoIcon, IslandIcon, PowerIcon, ReportIcon, SessionsIcon, WarnIcon } from "../shared/icons";
import "./traymenu.css";

/** 菜单窗口宽度（与 tauri.conf.json tray-menu.width 保持一致；高度自适应内容，无需同步） */
const MENU_WIDTH = 280;

/** 头部状态行的展示顺序（严重度优先：出错 > 等待输入 > 工作中）。
 *  空闲/离线不上桌——「没有事发生」的会话不占头部空间，总数在全空闲时交代一次；
 *  文案复用岛面板的 SESSION_META，与岛同一套词，零图例成本 */
const STATE_ORDER: SessionState[] = ["error", "waiting", "working"];

/** 单行菜单项：左图标 + 文案 + 右侧徽标（可选）；退出项红色悬停（danger） */
function Item({
  icon,
  label,
  badge,
  danger,
  onClick,
}: {
  icon: React.ReactNode;
  label: string;
  badge?: string;
  danger?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      className={`tm-item${danger ? " tm-item-danger" : ""}`}
      onClick={onClick}
    >
      <span className="tm-item-ico">{icon}</span>
      <span className="tm-item-label">{label}</span>
      {badge != null && <span className="tm-item-badge">{badge}</span>}
    </button>
  );
}

/** 明细行的数值摘要（M3-6）：Balance＝金额（跌破警戒线红字），Windows＝
 *  最紧张窗口百分比，无数据＝--；档位色类复用 App.css 的 text-normal/warn/danger */
function detailValue(a: IslandAccountView, t: Thresholds): React.ReactNode {
  if (a.balance) {
    // M3-7 按币种取警戒线：无对应线的币种不告急（不做凭空换算）
    const line = balanceWarnFor(t, a.balance.currency);
    const low = line != null && a.balance.total < line;
    return (
      <span className={low ? "text-danger" : undefined}>
        {fmtMoney(a.balance.currency, a.balance.total)}
      </span>
    );
  }
  const worst = [...a.quotas]
    .filter((q) => q.used_percent != null)
    .sort((x, y) => (y.used_percent ?? 0) - (x.used_percent ?? 0))[0];
  if (worst) {
    const pct = Math.round(worst.used_percent ?? 0);
    return (
      <span className={`text-${quotaLevel(pct, t.warn, t.danger)}`}>
        {windowLabel(worst.window_kind)} {pct}%
      </span>
    );
  }
  return <span className="tm-quota-none">--</span>;
}

/** 明细行的数据时间：余额/窗口快照 fetched_at 最大值（快照真实抓取时间，
 *  与额度页脚注同一口径；reset_at 是未来时点不可混用），无数据显 -- */
function detailTime(a: IslandAccountView): string {
  const times = [a.balance?.fetched_at ?? 0, ...a.quotas.map((q) => q.fetched_at ?? 0)].filter(
    (t) => t > 0,
  );
  return times.length ? fmtDT(Math.max(...times)) : "--";
}

export default function TrayMenu() {
  useTheme(); // 深浅主题跟随（与其他窗口同一套 Hook）
  const [snap, setSnap] = useState<IslandSnapshot | null>(null);
  const [islandVisible, setIslandVisible] = useState(true);
  // 每次托盘弹出自增：作为 key 重挂整棵树，重放 CSS 入场动画
  const [openSeq, setOpenSeq] = useState(0);
  // 额度警示阈值（与岛同源，缺省/清洗收敛 shared/types——四轮审查前此处
  // 手写 80/95/10/5 且无 warn<danger 交叉校验；归一告急判定用）
  const [thresholds, setThresholds] = useState<Thresholds>(DEFAULT_THRESHOLDS);
  // 全部实例明细展开态（M3-6）：点击额度行切换；菜单重挂（openSeq）自动收起
  const [detailOpen, setDetailOpen] = useState(false);
  // 高度自适应的测量目标（根元素高度由内容撑开，见 traymenu.css）
  const rootRef = useRef<HTMLDivElement>(null);

  const hideMenu = () => {
    getCurrentWindow().hide().catch(() => {});
  };

  useEffect(() => {
    // 快照与岛窗口同源：菜单开着时数据也随 10s 节拍保持新鲜
    const unSnap = listen<IslandSnapshot>("island-snapshot", (e) => setSnap(e.payload));
    // 每次托盘弹出前，Rust 推送最新的岛可见性（比本页内存态可靠）并触发入场动画
    const unShow = listen<{ island_visible: boolean }>("tray-menu-show", (e) => {
      setIslandVisible(e.payload.island_visible);
      setOpenSeq((n) => n + 1);
    });
    return () => {
      unSnap.then((f) => f());
      unShow.then((f) => f());
    };
  }, []);

  // 窗口高度自适应内容（M1-9 修复）：测量根元素实际高度（含 8px 透明边距）后
  // 以逻辑尺寸回写窗口——以后菜单项增减不必再回头改 tauri.conf.json 的高度。
  // 根元素高度由内容撑开（不锁 100vh），窗口尺寸变化不会反向触发观察器，无回环；
  // openSeq 变化时根元素随 key 重挂，故依赖它重挂观察器。conf 里的 height 仅是
  // 首帧兜底，挂载后即被测量值覆盖
  useLayoutEffect(() => {
    const el = rootRef.current;
    if (!el) return;
    const apply = () => {
      const h = el.offsetHeight;
      if (h > 0) getCurrentWindow().setSize(new LogicalSize(MENU_WIDTH, h)).catch(() => {});
    };
    apply();
    const ro = new ResizeObserver(apply);
    ro.observe(el);
    return () => ro.disconnect();
  }, [openSeq]);

  // 阈值挂载时读一次设置（额度行变色与余额告急判定用，与岛面板同一套；
  // 读不到用缺省，不阻塞渲染）。设置页修改后经 thresholds-changed 即时推送
  //（2026-09-29 分组优化，与岛窗口同款监听）
  useEffect(() => {
    invoke<Record<string, string>>("get_settings")
      .then((s) => setThresholds(sanitizeThresholds(s)))
      .catch(() => {});
    const un = listen<{
      warn?: number;
      danger?: number;
      balanceWarn?: number;
      balanceWarnUsd?: number;
    }>("thresholds-changed", (e) => {
      // 按字段各自校验合并（2026-10-03 审查修复：余额警戒线同步即时生效，
      // 原先只合并百分比段，余额线改了托盘红字要等重启）
      const p = e.payload ?? {};
      const fin = (v: number | undefined): v is number =>
        v != null && Number.isFinite(v);
      setThresholds((t) => ({
        ...t,
        ...(fin(p.warn) && fin(p.danger) && p.warn > 0 && p.danger <= 100 && p.warn < p.danger
          ? { warn: p.warn, danger: p.danger }
          : {}),
        ...(fin(p.balanceWarn) && p.balanceWarn >= 0 ? { balanceWarn: p.balanceWarn } : {}),
        ...(fin(p.balanceWarnUsd) && p.balanceWarnUsd >= 0
          ? { balanceWarnUsd: p.balanceWarnUsd }
          : {}),
      }));
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  // Esc 收起（失焦收起在 Rust 侧窗口事件，不依赖本页 JS 存活）
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") hideMenu();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // 动作分发后自隐：先 invoke 再 hide，避免点击反馈丢失；隐藏是幂等操作
  const act = (action: string) => {
    invoke("tray_menu_action", { action }).catch(() => {});
    hideMenu();
  };

  // 点击面板外的窗口空白（四周 8px 圆角边距）也收起——该区域属于本窗口，
  // 不会触发 Rust 侧失焦事件，需自行兜底
  const onRootClick = (e: React.MouseEvent<HTMLDivElement>) => {
    if (e.target === e.currentTarget) hideMenu();
  };

  // 头部三行各说一件事，零重复：①状态分布（只列非零「有事」状态，按严重度排序）；
  // ②今日消耗（渲染处）；③最紧张额度窗口
  const stateCounts = new Map<SessionState, number>();
  for (const s of snap?.sessions ?? []) {
    stateCounts.set(s.state, (stateCounts.get(s.state) ?? 0) + 1);
  }
  const busyChips = STATE_ORDER.map((state) => ({
    state,
    label: SESSION_META[state].label,
    count: stateCounts.get(state) ?? 0,
  })).filter((c) => c.count > 0);

  // ③额度行（M3-6 跨实例归一，06 §9.2）：余额跌破警戒线最优先红字告急
  //  （并列取更低余额）→ 窗口口径 used_percent 最高（沿用"并列取更短窗口"
  //  规则，扩展为跨实例选择器）；无任何额度数据整行隐藏（现状语义）。
  //  quota_exhausted（5h 用尽）保留旧优先级：红字"额度已用尽"替代百分比行
  const pick = snap != null ? tensestAccount(snap.accounts, thresholds) : null;
  // reason=balance 时 pick 实例必有本币警戒线（选择器 filter 保证），
  // 告急文案用其本币线拼装（M3-7 前为硬编码"¥"）
  const pickLine =
    pick?.reason === "balance"
      ? balanceWarnFor(thresholds, pick.account.balance.currency)
      : null;
  const showQuotaRow = pick != null || snap?.quota_exhausted === true;
  const quotaPct =
    pick?.quota?.used_percent != null
      ? Math.round(Math.min(100, Math.max(0, pick.quota.used_percent)))
      : null;
  const quotaLevelCls =
    quotaPct != null ? quotaLevel(quotaPct, thresholds.warn, thresholds.danger) : "normal";

  return (
    <div className="tm-root" key={openSeq} ref={rootRef} onClick={onRootClick}>
      <div className="tm-panel">
        {/* 信息头：三行各说一件事——状态分布 / 今日消耗 / 最紧张额度，零重复；
            degraded 时诚实降级提示 */}
        <div className="tm-head">
          <div className="tm-head-line1">
            {busyChips.length > 0 ? (
              <div className="tm-head-states">
                {busyChips.map((c) => (
                  <span key={c.state} className={`tm-chip chip-${c.state}`}>
                    <i className="tm-dot" />
                    <b>{c.count}</b>
                    {c.label}
                  </span>
                ))}
              </div>
            ) : (
              <span className="tm-head-quiet">
                {snap
                  ? snap.sessions.length > 0
                    ? `${snap.sessions.length} 个会话全部空闲`
                    : "未检测到会话"
                  : "启动中…"}
              </span>
            )}
            {snap?.degraded && (
              <span className="tm-head-warn">
                <WarnIcon />
              </span>
            )}
          </div>
          <div className={`tm-head-line2${snap?.degraded ? " tm-head-warn-text" : ""}`}>
            {snap?.degraded
              ? "数据降级中，统计可能不完整"
              : snap && snap.today_calls > 0
                ? /* 不带单位后缀（2026-10-10 审查统一）：fmtTokens 已带 K/M 缩写，
                     岛面板/胶囊同口径均无后缀；需点明单位时全项目统一小写 token */
                  `今日 ${fmtTokens(snap.today_tokens)} · ${snap.today_calls} 次调用`
                : "今日暂无用量"}
          </div>
          {showQuotaRow && (
            /* 额度行（M3-6）：跨实例归一摘要，点击展开/收起全部实例明细
               （明细行才跳额度页；展开态不收起菜单，供逐行点击） */
            <div
              className="tm-head-line2 tm-head-quota tm-head-quota-link"
              role="button"
              tabIndex={0}
              title={detailOpen ? "点击收起全部实例明细" : "点击展开全部实例明细"}
              onClick={() => setDetailOpen((v) => !v)}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  setDetailOpen((v) => !v);
                }
              }}
            >
              {snap?.quota_exhausted ? (
                <span className="tm-quota-exhausted">额度已用尽，等待窗口重置</span>
              ) : pick?.reason === "balance" ? (
                <>
                  <span className="tm-quota-urgent">
                    {accountLabel(pick.account.kind_name, pick.account.alias)} 余额{" "}
                    {fmtMoney(pick.account.balance.currency, pick.account.balance.total)}
                  </span>
                  <span className="tm-quota-sub">
                    {/* pickLine 理论非空（balance 型不变式）；兜底时整段省略
                        而非留「低于警戒线  · 点击展开」破句（2026-10-10 审查修复） */}
                    （{pickLine != null ? `低于警戒线 ${fmtMoney(pick.account.balance.currency, pickLine)} · ` : ""}点击展开）
                  </span>
                </>
              ) : (
                pick?.quota && (
                  <>
                    额度 <b className={`text-${quotaLevelCls}`}>{quotaPct}%</b>
                    <span className="tm-quota-sub">
                      （{windowLabel(pick.quota.window_kind)} ·{" "}
                      {accountLabel(pick.account.kind_name, pick.account.alias)}）
                      {pick.quota.reset_at != null && (
                        <> · {fmtCountdownCN(pick.quota.reset_at)}后重置</>
                      )}
                    </span>
                  </>
                )
              )}
            </div>
          )}
          {showQuotaRow && detailOpen && snap && (
            /* 全部实例明细（M3-6，06 §9.2）：色点＋别名＋数值摘要＋更新时间，
               行点击打开额度窗口并直达该实例卡片（#12：先 emit 深链再开窗，
               额度页常驻已监听 goto-quota-account；act 后自隐保反馈） */
            <div className="tm-quota-detail">
              {snap.accounts.map((a) => (
                <button
                  key={a.id}
                  type="button"
                  className="tm-quota-item"
                  title="点击打开额度窗口并定位到该实例"
                  onClick={() => {
                    emit("goto-quota-account", { accountId: a.id }).catch(() => {});
                    act("quotas");
                  }}
                >
                  <i className="tm-quota-dot" style={{ background: a.color }} />
                  <span className="tm-quota-alias">
                    {accountLabel(a.kind_name, a.alias)}
                  </span>
                  <span className="tm-quota-val">{detailValue(a, thresholds)}</span>
                  <span className="tm-quota-time">{detailTime(a)}</span>
                </button>
              ))}
            </div>
          )}
        </div>
        <div className="tm-sep" />
        <Item
          icon={<IslandIcon />}
          label={islandVisible ? "隐藏灵动岛" : "显示灵动岛"}
          badge={islandVisible ? "可见" : "已隐藏"}
          onClick={() => act("toggle")}
        />
        <Item icon={<ReportIcon />} label="报表" onClick={() => act("report")} />
        <Item icon={<SessionsIcon />} label="会话" onClick={() => act("sessions")} />
        {/* 额度项与报表同级（06 §8.1，M3-5）：常驻入口，不依赖信息头额度行
            是否有数据——零实例时也能到达额度页空态引导 */}
        <Item icon={<CoinIcon />} label="额度" onClick={() => act("quotas")} />
        <Item icon={<GearIcon />} label="设置" onClick={() => act("settings")} />
        <Item icon={<InfoIcon />} label="关于" onClick={() => act("about")} />
        <div className="tm-sep" />
        <Item icon={<PowerIcon />} label="退出" danger onClick={() => act("quit")} />
      </div>
    </div>
  );
}
