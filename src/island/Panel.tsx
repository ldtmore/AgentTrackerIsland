/**
 * 展开面板（2026-09-18 展示改造 P1-P11；M1-10 收口）：
 * 今日汇总条（活跃/今日消耗/调用次数 + 报表入口）
 * → 会话列表（活跃区/历史展开区各设条数上限，溢出显示「查看更多会话」
 *   链接直达会话窗口；历史区默认折叠为一行摘要，点击展开）
 * → 额度区（M3-6 多实例轮播＋点状指示器：Windows 口径 chip 行、
 *   Balance 口径金额 chip；单实例保持 chip 样式零回退；2026-10-03
 *   密度对齐「四行并两行」：标题行兼载实例身份与轮播点，总高对齐
 *   一张会话卡——消除面板「上紧下松」的密度落差）。
 * 状态文案/标题回退/排序口径抽至 shared/sessionDisplay（与会话窗口同源）。
 * 卡片两行：第一行 = 状态·相对时间 + 会话标题（主文案）；
 * 第二行 = 模型/项目徽章 + token（悬浮展示四项拆解）。
 * 点击会话卡片的跳转行为由 T10 接入
 */
import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import type { IslandAccountView, IslandSnapshot, SessionView, Thresholds } from "../shared/types";
import {
  accountUrgent,
  balanceWarnFor,
  errorReason,
  resetPhrase,
  accountLabel,
  fmtRelative,
  fmtTokens,
  instanceColor,
  quotaLevel,
  shortAlias,
  windowLabel,
} from "../shared/types";
import { fmtDT, fmtMoney } from "../shared/format";
import { orderAgentIds } from "../shared/agentOrder";
import { agentFullName, agentShortName, cardTitle, displayState, isJumpable, sortHistory, sortSessions } from "../shared/sessionDisplay";
import AgentBadge from "../shared/AgentBadge";
import Tip from "../shared/Tip";
import { ChevronIcon, ChipIcon, CoinIcon, FolderIcon, ReportIcon, WarnIcon } from "../shared/icons";

/** 活跃区卡片上限（宽松：会话少时保持一眼全览；超出部分走会话窗口） */
const ACTIVE_LIMIT = 12;

/** 历史区展开后的卡片上限（同样溢出直达会话窗口） */
const HISTORY_LIMIT = 20;

/** 多实例轮播间隔（毫秒，06 §9.1：代码常量，不做设置项） */
const CAROUSEL_INTERVAL_MS = 6_000;
/** 手动切换圆点后自动轮播的暂停时长（毫秒）：避免刚切走即跳回（06 §9.1） */
const CAROUSEL_PAUSE_MS = 30_000;

/** Agent 筛选菜单：悬浮展开延迟（毫秒）。0=立即跟手；实测误触多可调 100 做意图检测 */
const FILTER_OPEN_DELAY_MS = 0;
/** Agent 筛选菜单：移出触发器+菜单整体区域后的宽限时长（毫秒）——
 *  覆盖「从触发器斜移进下方菜单」的路径，防止闪关（业界 hover 菜单标准做法）。
 *  菜单宽度在 CSS（.history-menu width:190px）定义，定位按实测尺寸计算 */
const FILTER_CLOSE_DELAY_MS = 200;

/** 打开会话窗口（溢出链接与标题行入口共用） */
function openSessions() {
  invoke("show_sessions_window").catch(() => {});
}

/** 打开额度窗口（M3-5：面板额度区标题行入口；详情管理在额度页/设置页闭环） */
function openQuotas() {
  invoke("show_quotas_window").catch(() => {});
}

/** 「查看更多会话」溢出链接（所有者拍板文案；hover 提示完整语义）。
 *  M2-UX-1：历史区筛选中跳转时带 Agent 预筛选（emit 全局事件，
 *  会话窗口监听后自动选中该 Agent——面板筛选体验在窗口内闭环） */
function MoreLink({ hidden, agent }: { hidden: number; agent?: string | null }) {
  const open = () => {
    if (agent) {
      emit("sessions-prefilter", agent).catch(() => {});
    }
    openSessions();
  };
  return (
    <Tip content="点击查看更多会话数据">
      <button className="more-link" onClick={open}>
        查看更多会话
        {hidden > 0 && <span className="history-latest">还有 {hidden} 个</span>}
      </button>
    </Tip>
  );
}

function SessionCard({ s }: { s: SessionView }) {
  const disp = displayState(s.state, s.last_activity_at);
  const project = s.project_dir?.split(/[\\/]/).filter(Boolean).pop() ?? "";
  const model = s.model;
  const title = cardTitle(s.title, s.project_dir, s.id);
  // 状态 + 相对时间（P4）：时间量感是"空闲/已结束"可读的关键
  const timeText = fmtRelative(s.last_activity_at);
  const stateText = timeText && !disp.ended ? `${disp.label} · ${timeText}` : disp.label;
  // 出错原因（P7）：error_type 已采集未用——透出具体错因而非干巴巴"出错"
  const stateFinal = s.state === "error" ? errorReason(s.error_type) : stateText;
  // 跳转新鲜度（07-UX 1.5，与会话窗口同一条规则）：仅近 30 分钟有活动的会话
  // 绑定跳转——陈旧/已结束会话的进程大概率已退出，focus_session 必然未命中，
  // 「点了没反应」正是会话页要防的负体验，且面板是更高频入口。
  // 已知取舍（两处统一）：进程还开着但超 30 分钟的会话在面板同样不可点
  const jumpable = isJumpable(s.last_activity_at);
  // 点击跳转：激活该会话对应的终端/IDE 窗口（T10；未命中静默失败）
  const focus = () => {
    invoke("focus_session", { sessionId: s.id }).catch(() => {});
  };
  return (
    <div
      className={`card${disp.ended ? " card-ended" : ""}${jumpable ? "" : " card-nojump"}`}
      data-session-id={s.id}
      {...(jumpable
        ? {
            // 键盘可达（2026-10-10 审查 a11y，对齐 rp-dim-row/st-agent 卡先例）：
            // 会话卡是面板主交互件，仅 onClick 对键盘用户整体不可达
            onClick: focus,
            role: "button" as const,
            tabIndex: 0,
            onKeyDown: (e: React.KeyboardEvent) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                focus();
              }
            },
          }
        : {})}
    >
      <div className="card-main">
        <div className="card-line1">
          {/* 身份徽章（2026-09-30 徽章统一改版·方案 X）：行首最左身份锚，
              短名＋身份色胶囊（悬浮出全名）；行首旧状态点移除——
              状态右移与文字成组，圆点从此只出现在状态旁 */}
          <AgentBadge agent={s.agent} name={agentShortName(s.agent)} title={agentFullName(s.agent)} />
          {/* 悬浮展示完整标题（截断有省略号暗示，气泡只作补充——G2 规则③） */}
          <Tip content={title}>
            <span className="card-title">{title}</span>
          </Tip>
          {/* 状态组：点＋文字右对齐（点语言不变：呼吸/快闪/暗淡，仅位置移动） */}
          <span className={`card-state${s.state === "error" ? " text-error" : ""}`}>
            <i className={`dot ${disp.dot}`} />
            {stateFinal}
          </span>
        </div>
        <div className="card-line2">
          <Tip content={model ? `使用的模型：${model}` : "尚未捕获该会话的模型调用"}>
            <span className="card-tag">
              <ChipIcon />
              {model ?? "--"}
            </span>
          </Tip>
          <Tip content={s.project_dir ?? "无法识别工作目录"}>
            <span className="card-tag">
              <FolderIcon />
              {project || "--"}
            </span>
          </Tip>
          <Tip
            content={
              <div className="tip-breakdown">
                <div>
                  输入 {fmtTokens(s.input_tokens)} · 输出 {fmtTokens(s.output_tokens)}
                </div>
                <div>
                  缓存读 {fmtTokens(s.cache_read_tokens)} · 缓存写 {fmtTokens(s.cache_creation_tokens)}
                </div>
                <div className="tip-dim">本会话累计 · 含缓存，与账单同口径</div>
              </div>
            }
          >
            <span className="card-tokens">{fmtTokens(s.session_tokens)}</span>
          </Tip>
        </div>
      </div>
    </div>
  );
}

/** 历史区（P2 折叠 + M2-UX-1 筛选器）：折叠头**点击**展开/收起历史卡片；
 *  标题行右端 Agent 筛选器为**纯 hover 交互**且**仅展开态可见**（渐进披露：
 *  收起态筛选是死路——看不见列表的筛选没有意义；收起时自动清除筛选）——
 *  悬浮即展开菜单、点选即应用并关闭、移出触发器+菜单区域宽限 200ms 自动
 *  收起（Esc 兜底）。菜单是 **portal 浮层**（挂 body + fixed 定位，照抄 Tip
 *  范式：不受会话区滚动容器 overflow 裁剪、不推挤下方卡片布局；会话区滚动
 *  即自动关闭）。与岛「悬停展开面板」的 hover 基因一致；列表纯最近活动
 *  降序（分流后活跃优先无语义）；筛选是临时意图：面板收起随组件卸载自动复位 */
function HistorySection({ all }: { all: SessionView[] }) {
  const [open, setOpen] = useState(false); // 默认折叠（用户拍板）：点击折叠头切换
  const [filterOpen, setFilterOpen] = useState(false); // 筛选菜单展开（hover 驱动）
  const [agent, setAgent] = useState<string | null>(null); // 当前筛选（null=全部）
  const filterBtnRef = useRef<HTMLButtonElement>(null); // 触发器：浮层定位锚点
  const menuRef = useRef<HTMLDivElement>(null); // 浮层：两段式测量的真实尺寸来源
  const [menuPos, setMenuPos] = useState<{ x: number; y: number } | null>(null); // null=待测量（隐藏渲染）
  const closeTimer = useRef<number | undefined>(undefined);
  const openTimer = useRef<number | undefined>(undefined);

  // Esc 兜底关闭（hover 无键盘入口，至少保证可退出）
  useEffect(() => {
    if (!filterOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setFilterOpen(false);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [filterOpen]);

  // 卸载清理：面板收起时组件卸载，未到期的开/关计时一并作废
  useEffect(() => {
    return () => {
      window.clearTimeout(closeTimer.current);
      window.clearTimeout(openTimer.current);
    };
  }, []);

  // 浮层特有：会话区滚动即关闭（内容滚走菜单不能钉在原地错位）。
  // capture 监听捕获内部滚动；菜单自身滚动（14+ 家超 max-height）除外
  useEffect(() => {
    if (!filterOpen) return;
    const onScroll = (e: Event) => {
      if (menuRef.current?.contains(e.target as Node)) return;
      setFilterOpen(false);
    };
    document.addEventListener("scroll", onScroll, true);
    return () => document.removeEventListener("scroll", onScroll, true);
  }, [filterOpen]);

  // 浮层两段式定位（照抄 Tip 模式）：先隐藏渲染量真实高度，再定最终坐标——
  // 右缘对齐触发器右缘、纵向贴下缘 +4px，下方放不下向上翻转，整体夹进视口
  useLayoutEffect(() => {
    if (!filterOpen || menuPos) return;
    const btn = filterBtnRef.current;
    const menu = menuRef.current;
    if (!btn || !menu) return;
    const br = btn.getBoundingClientRect();
    const mr = menu.getBoundingClientRect();
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const x = Math.min(Math.max(br.right - mr.width, 8), vw - mr.width - 8);
    const belowY = br.bottom + 4;
    const aboveY = br.top - 4 - mr.height;
    const y =
      belowY + mr.height <= vh - 8
        ? belowY
        : aboveY >= 8
          ? aboveY
          : Math.min(belowY, vh - mr.height - 8);
    setMenuPos({ x, y });
  }, [filterOpen, menuPos]);

  /** 取消在途关闭计时：宽限期内回到区域内（含移入菜单）即撤销关闭 */
  const cancelClose = () => window.clearTimeout(closeTimer.current);

  /** 悬浮触发器：取消在途关闭计时，按延迟常量展开菜单（每次展开重新测量定位） */
  const openMenu = () => {
    cancelClose();
    window.clearTimeout(openTimer.current);
    if (FILTER_OPEN_DELAY_MS > 0) {
      openTimer.current = window.setTimeout(() => {
        setMenuPos(null);
        setFilterOpen(true);
      }, FILTER_OPEN_DELAY_MS);
    } else {
      setMenuPos(null);
      setFilterOpen(true);
    }
  };

  /** 移出整体区域：宽限后关闭；宽限期内回到区域内（含移入菜单）则取消 */
  const armClose = () => {
    window.clearTimeout(closeTimer.current);
    closeTimer.current = window.setTimeout(() => setFilterOpen(false), FILTER_CLOSE_DELAY_MS);
  };

  // 按 Agent 聚合已结束计数；条目按用户拖拽序展示（2026-10-03 排序决策：
  // 放弃旧「会话数降序常用自浮」——动态序使条目位置随使用漂移，破坏
  // 悬停快选的空间记忆；用户序与全应用枚举位一致（设置页卡片/贴边分段条/
  // 报表下拉），"全部"仍置顶，计数降为右侧提示）
  const counts = new Map<string, number>();
  for (const s of all) counts.set(s.agent, (counts.get(s.agent) ?? 0) + 1);
  const menuAgents = orderAgentIds([...counts.keys()]).map(
    (id) => [id, counts.get(id) ?? 0] as const,
  );

  const list = sortHistory(agent ? all.filter((s) => s.agent === agent) : all);
  const latest = list[0];
  const latestText = latest
    ? `${cardTitle(latest.title, latest.project_dir, latest.id)} · ${fmtRelative(latest.last_activity_at)}`
    : "";
  /** 点选菜单项：应用筛选并立即关闭（选完即关） */
  const pick = (id: string | null) => {
    setAgent(id);
    setFilterOpen(false);
  };

  /** 折叠头点击：展开/收起历史卡片。收起时同步清除筛选并关闭菜单——
   *  筛选器仅在展开态可见（渐进披露），收起态若残留筛选会形成
   *  「看得到计数却改不了筛选」的反向死角（M2-UX-1 交互细化） */
  const toggleOpen = () => {
    if (open) {
      setAgent(null);
      setFilterOpen(false);
    }
    setOpen(!open);
  };
  return (
    <div>
      <div className={`history-row${filterOpen ? " menu-open" : ""}`}>
        <Tip
          content={
            <div className="tip-breakdown">
              <div>已结束的历史会话，默认收起</div>
              {agent ? (
                <div className="tip-dim">
                  当前筛选：仅 {agentFullName(agent)}，命中 {list.length} / 共 {all.length} 个
                </div>
              ) : (
                <div className="tip-dim">共 {all.length} 个 · 点击展开 / 收起列表</div>
              )}
            </div>
          }
        >
          <button className="history-toggle" onClick={toggleOpen}>
            {/* 折叠箭头 SVG 化（五轮审查补件）：chevron-open 仍转 span 本体，
                既有 150ms 旋转过渡不变；右向＝收起、随 open 转为下向 */}
            <span className={`chevron${open ? " chevron-open" : ""}`}>
              <ChevronIcon dir="right" />
            </span>
            已结束 {list.length} 个{agent && <span> · 仅 {agentFullName(agent)}</span>}
            {!open && latestText && <span className="history-latest">最近：{latestText}</span>}
          </button>
        </Tip>
        {open && (
          <button
            ref={filterBtnRef}
            className={`history-filter${agent ? " active" : ""}${filterOpen ? " open" : ""}`}
            aria-expanded={filterOpen}
            aria-label="按 Agent 筛选已结束会话"
            onMouseEnter={openMenu}
            onMouseLeave={armClose}
          >
            {/* 触发钮＝紧凑位：短名徽章（旧 AGENT_BADGE 缩写表已废）；箭头 SVG 化（五轮审查补件） */}
            {agent ? (
              <AgentBadge agent={agent} name={agentShortName(agent)} title={agentFullName(agent)} />
            ) : (
              "全部"
            )}{" "}
            <ChevronIcon dir="down" />
          </button>
        )}
      </div>
      {filterOpen &&
        createPortal(
          <div
            ref={menuRef}
            className="history-menu"
            style={{
              left: menuPos?.x ?? -9999,
              top: menuPos?.y ?? -9999,
              visibility: menuPos ? "visible" : "hidden",
              animation: menuPos ? undefined : "none", // 定位完成前不播入场动画（防动画在隐藏阶段播完）
            }}
            onMouseEnter={cancelClose}
            onMouseLeave={armClose}
          >
            <button
              className={`history-menu-row${agent === null ? " on" : ""}`}
              onClick={() => pick(null)}
            >
              <span className="history-menu-name">全部 Agent</span>
              <span className="history-menu-count">{all.length}</span>
            </button>
            {menuAgents.map(([id, n]) => (
              <button
                key={id}
                className={`history-menu-row${agent === id ? " on" : ""}`}
                onClick={() => pick(id)}
              >
                {/* 菜单＝宽敞位：全名徽章（身份即名字，色点已退役） */}
                <AgentBadge agent={id} name={agentFullName(id)} size="lg" />
                <span className="history-menu-count">{n}</span>
              </button>
            ))}
          </div>,
          document.body,
        )}
      {open && (
        <>
          {list.slice(0, HISTORY_LIMIT).map((s) => (
            <SessionCard key={s.id} s={s} />
          ))}
          {list.length > HISTORY_LIMIT && (
            <MoreLink hidden={list.length - HISTORY_LIMIT} agent={agent} />
          )}
        </>
      )}
    </div>
  );
}

/** 单个额度窗口 chip（2026-09-18 二次优化）：迷你条 + 百分比一行排布，
 *  重置时间等细节收敛进悬浮提示（官方语义见各窗口 tooltip 文案） */
function QuotaChip({
  label,
  fullName,
  usedPercent,
  resetAt,
  thresholds,
}: {
  /** 行内短标签：5h / 7d（官方窗口机制的最短写法） */
  label: string;
  /** 悬浮提示用的中文名 */
  fullName: string;
  usedPercent: number | null;
  resetAt: number | null;
  thresholds: Thresholds;
}) {
  const pct = usedPercent != null ? Math.min(100, Math.max(0, usedPercent)) : 0;
  const level =
    usedPercent != null
      ? quotaLevel(pct, thresholds.warn, thresholds.danger)
      : "normal";
  // 悬浮提示单行无歧义："5 小时额度：已用 2% · 预计 1 小时 34 分钟后重置"
  let status: string;
  if (usedPercent == null) {
    status = "暂无数据，约 5 分钟后自动重试";
  } else if (resetAt == null) {
    status = pct === 0 ? "窗口刚刷新，暂无用量" : `已用 ${Math.round(pct)}% · 重置时间待供应商返回`;
  } else {
    status = `已用 ${Math.round(pct)}% · ${resetPhrase(resetAt)}`;
  }
  return (
    <Tip content={`${fullName}：${status}`}>
      <span className="quota-chip">
        <span className="quota-label">{label}</span>
        <div className="quota-bar">
          {/* scaleX（合成器属性）替代 width（布局属性）：变化平滑且不触发布局重排 */}
          <div
            className={`quota-fill fill-${level}`}
            style={{ transform: `scaleX(${pct / 100})` }}
          />
        </div>
        <span className={`quota-pct text-${level}`}>
          {usedPercent != null ? `${Math.round(pct)}%` : "--"}
        </span>
      </span>
    </Tip>
  );
}

/** 指示器圆点的悬浮提示（06 §9.1："厂商 · 别名 ＋ 该实例额度摘要"）。
 *  语义分行（2026-10-03 悬浮提示优化）：数据主体一行（身份＋余额/窗口摘要
 *  随手扫一眼读完整状态），动作提示独立弱色行——与历史折叠行气泡同款
 *  主次分级（主行 div ＋ tip-dim 弱色行） */
function dotTip(a: IslandAccountView, t: Thresholds): ReactNode {
  const parts = [accountLabel(a.kind_name, a.alias)];
  if (a.balance) {
    // M3-7 按币种取警戒线：本币线拼装文案（无对应线不告急，不做凭空换算）
    const line = balanceWarnFor(t, a.balance.currency);
    const low = line != null && a.balance.total < line;
    parts.push(
      `余额 ${fmtMoney(a.balance.currency, a.balance.total)}${
        low ? `（低于警戒线 ${fmtMoney(a.balance.currency, line)}）` : ""
      }`,
    );
  }
  for (const q of a.quotas) {
    parts.push(`${windowLabel(q.window_kind)} 已用 ${Math.round(q.used_percent ?? 0)}%`);
  }
  if (!a.balance && a.quotas.length === 0) parts.push("暂无额度数据");
  return (
    <div className="tip-breakdown">
      <div>{parts.join(" ")}</div>
      <div className="tip-dim">点击切换到该实例</div>
    </div>
  );
}

/** 实例额度体（轮播帧内容，M3-6）：Windows 口径＝双窗口 chip 行（与单实例
 *  旧版同款）；Balance 口径＝金额 chip（低于警戒线 danger 色）；其余口径
 *  （LocalEstimate，第五批才有生产者）＝灰字占位预留臂 */
function AccountQuotaBody({
  account,
  thresholds,
}: {
  account: IslandAccountView;
  thresholds: Thresholds;
}) {
  if (account.quota_kind === "balance") {
    const b = account.balance;
    if (!b) {
      return (
        <Tip content="暂无数据，约 5 分钟后自动重试">
          <span className="quota-money quota-money-off">余额 --</span>
        </Tip>
      );
    }
    // M3-7 按币种取警戒线：本币线拼装文案（无对应线不告急，不做凭空换算）
    const line = balanceWarnFor(thresholds, b.currency);
    const low = line != null && b.total < line;
    const tip = [
      accountLabel(account.kind_name, account.alias),
      `余额 ${fmtMoney(b.currency, b.total)}${
        low ? `（低于警戒线 ${fmtMoney(b.currency, line)}）` : ""
      }${b.total < 0 ? "，账户欠费" : ""}`,
      b.granted != null
        ? `赠金 ${fmtMoney(b.currency, b.granted)} · 现金 ${fmtMoney(b.currency, b.total - b.granted)}`
        : "",
      `最近数据 ${fmtDT(b.fetched_at)}`,
    ]
      .filter(Boolean)
      .join("\n");
    return (
      <Tip content={tip}>
        <span className={`quota-money${low ? " quota-money-low" : ""}`}>
          余额 {fmtMoney(b.currency, b.total)}
        </span>
      </Tip>
    );
  }
  // Windows／LocalEstimate 口径共用 chip 行（M3-12）：推算实例加"推算"小标，
  // 7d 行 used_percent 为 null＝限额未公开，QuotaChip 显"--"
  if (account.quota_kind === "windows" || account.quota_kind === "local_estimate") {
    const est = account.quota_kind === "local_estimate";
    const q5h = account.quotas.find((q) => q.window_kind === "5h");
    const qWeek = account.quotas.find((q) => q.window_kind === "weekly");
    return (
      <div className="quota-row">
        {est && (
          <Tip content="本地推算值：限额为社区测算值，仅统计 Claude Code 用量">
            <span className="pc-stale">推算</span>
          </Tip>
        )}
        <QuotaChip
          label="5h"
          fullName="5 小时额度"
          usedPercent={q5h?.used_percent ?? null}
          resetAt={q5h?.reset_at ?? null}
          thresholds={thresholds}
        />
        <span className="quota-sep" />
        <QuotaChip
          label="7d"
          fullName="7 天额度"
          usedPercent={qWeek?.used_percent ?? null}
          resetAt={qWeek?.reset_at ?? null}
          thresholds={thresholds}
        />
      </div>
    );
  }
  // 未知口径（注册表缺厂商等异常路径）用通用灰字，不冒充实了哪种口径
  return <span className="quota-money quota-money-off">暂无额度数据</span>;
}

/** 多实例轮播（M3-6，06 §9.1 D7 所有者方案）：6s 自动前进一帧；点状指示器＝
 *  实例色（同厂商多实例色阶压暗区分），点击圆点立即切换并暂停自动轮播 30s；
 *  不在当前帧的实例告急（窗口达 danger／余额跌破警戒线）→ 圆点红色脉冲
 *  （被动提示不抢焦点，红线⑤）。单实例不渲染指示器（chip 样式零回退）。
 *  2026-10-03 密度对齐（四行并两行）：区头标题行移入本组件渲染——左槽
 *  「额度」eyebrow ＋当前帧身份（色点＋别名，换帧随 key 重挂淡入），右槽
 *  轮播点＋「额度」链接（同为控制/导航职能，语义同层）；帧体独占第二行。
 *  额度区总高对齐一张会话卡，消除「上紧下松」的密度落差 */
function QuotaCarousel({
  accounts,
  thresholds,
  link,
  hoverRef,
}: {
  accounts: IslandAccountView[];
  thresholds: Thresholds;
  /** 区头右槽的「额度」链接（Panel 传入，与零实例态同一份 JSX） */
  link: ReactNode;
  /** 悬停暂停标志（07-UX 3.8）：Panel 在 .panel-quota 上挂鼠标进出维护，
   *  ref 现读不触发渲染——悬停读数不被翻走，移出下一拍即恢复 */
  hoverRef: { current: boolean };
}) {
  const [idx, setIdx] = useState(0);
  // 手动切换的暂停截止时刻（ref：心跳每次触发现读，不需触发渲染）
  const resumeAtRef = useRef(0);

  // 轮播心跳：每 6s 前进一帧；暂停期内空转（点击切换的 30s 惩罚档＋悬停的
  // 独立标志位并存互不污染），恢复点有 6s 粒度——悬停移出后下一拍即恢复，
  // 不吃 30s 惩罚（07-UX 3.8）
  useEffect(() => {
    if (accounts.length <= 1) return;
    const t = window.setInterval(() => {
      if (Date.now() < resumeAtRef.current) return;
      if (hoverRef.current) return;
      setIdx((i) => (i + 1) % accounts.length);
    }, CAROUSEL_INTERVAL_MS);
    return () => window.clearInterval(t);
  }, [accounts.length, hoverRef]);

  // 展示集缩小（设置页移出/停用）时夹回合法帧
  const curIdx = Math.min(idx, accounts.length - 1);
  const cur = accounts[curIdx];

  // 同厂商多实例序号 → 色阶偏移（每轮渲染重算，accounts 顺序稳定故色不漂移）
  const shadeSeq = new Map<string, number>();
  const shadeOf = (a: IslandAccountView) => {
    const n = shadeSeq.get(a.kind_id) ?? 0;
    shadeSeq.set(a.kind_id, n + 1);
    return n;
  };

  /** 点圆点：立即换帧并暂停自动轮播 30s */
  const switchTo = (i: number) => {
    resumeAtRef.current = Date.now() + CAROUSEL_PAUSE_MS;
    setIdx(i);
  };

  return (
    <>
      {/* 区头标题行（密度对齐改版）：左槽＝「额度」eyebrow ＋当前帧身份；
          右槽＝轮播点＋「额度」链接。身份与帧体各自 key 重挂播放淡入，
          标题行本身不重挂——轮播点与链接换帧时保持稳定不闪烁 */}
      <div className="panel-title">
        <span className="quota-title-left">
          {/* 区段名不参与收缩（flex:none 在 CSS）：别名长时省略号兜底，
              固定文字若被按比例压缩，中文两字会换行成竖排 */}
          <span className="quota-title-label">额度</span>
          {/* 帧头身份（2026-09-30 降噪）：别名全局唯一（D14 升级）可独立承载
              身份，厂商名收进悬浮；色点承载厂商色彩，默认别名＝厂商名时不再
              「智谱 GLM · 智谱 GLM」全重复。托盘等纯文字场景仍保留「厂商·别名」 */}
          <span className="quota-frame-id" key={cur.id} title={accountLabel(cur.kind_name, cur.alias)}>
            <i className="quota-frame-dot" style={{ background: cur.color }} />
            {shortAlias(cur.alias)}
          </span>
        </span>
        <span className="quota-title-right">
          {accounts.length > 1 && (
            <div className="qdots">
              {accounts.map((a, i) => {
                const urgent = i !== curIdx && accountUrgent(a, thresholds);
                return (
                  <Tip key={a.id} content={dotTip(a, thresholds)}>
                    <button
                      type="button"
                      className={`qdot${i === curIdx ? " qdot-on" : ""}${urgent ? " qdot-urgent" : ""}`}
                      /* 必须用 backgroundColor 长属性：background 简写会把 CSS 的
                         background-clip: content-box 重置回 border-box，导致整颗
                         20px 命中区被涂成大圆点（2026-10-03 四轮验收实测翻车点） */
                      style={{ backgroundColor: instanceColor(a.color, shadeOf(a)) }}
                      aria-label={`切换到 ${accountLabel(a.kind_name, a.alias)}`}
                      onClick={() => switchTo(i)}
                    />
                  </Tip>
                );
              })}
            </div>
          )}
          {link}
        </span>
      </div>
      {/* 帧体：key＝实例 id 重挂重放淡入动画；Windows chip 行与 Balance 金额行
          同高（min-height 18px），轮播切换不跳高 */}
      <div className="quota-frame-body" key={cur.id}>
        <AccountQuotaBody account={cur} thresholds={thresholds} />
      </div>
    </>
  );
}

/** 今日汇总条（P1）：面板首行回答"今天整体怎么样"，报表按钮直达报表窗口。
 *  批次二（F8）：删「活跃 N」——与标题行「会话 · N/M」的活跃数同源重复；
 *  degraded 琥珀小标（状态语言矩阵）：采集异常在面板不再沉默 */
function SummaryBar({ snap }: { snap: IslandSnapshot }) {
  const openReport = () => {
    invoke("show_report_window").catch(() => {});
  };
  return (
    <div className="sum-bar">
      <Tip content={`今日 ${fmtTokens(snap.today_tokens)}（含缓存，账单口径）`}>
        <span className="sum-text">
          {snap.today_calls > 0 ? (
            <>
              今日 <b>{fmtTokens(snap.today_tokens)}</b> · {snap.today_calls} 次调用
            </>
          ) : (
            "今日暂无用量"
          )}
        </span>
      </Tip>
      <span className="sum-actions">
        {snap.degraded && (
          <Tip content={"采集源连续失败，数据可能滞后\n胶囊上亦有红标提示"}>
            <span className="sum-degraded">
              <WarnIcon />
              采集异常
            </span>
          </Tip>
        )}
        <Tip content="打开报表窗口">
          <button className="sum-link" onClick={openReport}>
            <ReportIcon />
            报表
          </button>
        </Tip>
      </span>
    </div>
  );
}

export default function Panel({
  snap,
  thresholds,
  onNaturalHeight,
}: {
  snap: IslandSnapshot;
  thresholds: Thresholds;
  /** 内容自然高度上报（App 据此调窗口高度，超出上限转会话区内部滚动） */
  onNaturalHeight?: (h: number) => void;
}) {
  // 相对时间每 30s 重渲染一次（快照 10s 一刷，本地再兜一层"刚刚→N 分钟前"的推进）
  const [, force] = useState(0);
  useEffect(() => {
    const t = setInterval(() => force((n) => n + 1), 30_000);
    return () => clearInterval(t);
  }, []);

  // 面板自然高度测量（窗口高度自适应的数据源，2026-09-18 展示改造）：
  // 面板被 max-height 钳制时，溢出被会话区内部滚动吸收，面板自身高度不再反映
  // 真实内容量，须用"会话区 scrollHeight − clientHeight"的溢出量补偿上报，
  // 否则内容变多时窗口停在原高不再生长
  const panelRef = useRef<HTMLDivElement>(null);
  const sessionsRef = useRef<HTMLDivElement>(null);
  // 额度区悬停标志（07-UX 3.8）：true 时额度轮播心跳空转不翻帧，传给
  // QuotaCarousel 现读；鼠标进出在 .panel-quota 容器上维护
  const quotaHoverRef = useRef(false);
  const reportHeight = useCallback(() => {
    if (!onNaturalHeight) return;
    const panel = panelRef.current;
    const sessions = sessionsRef.current;
    if (!panel || !sessions) return;
    const overflow = Math.max(0, sessions.scrollHeight - sessions.clientHeight);
    onNaturalHeight(Math.round(panel.getBoundingClientRect().height) + overflow);
  }, [onNaturalHeight]);

  // 每次渲染后上报：快照更新、历史区开合、相对时间推进都可能改变内容量
  useLayoutEffect(() => {
    reportHeight();
  });

  // 窗口尺寸变化（钳制边界移动）不经过 React 渲染，用 ResizeObserver 兜住
  useEffect(() => {
    const panel = panelRef.current;
    const sessions = sessionsRef.current;
    if (!panel || !sessions) return;
    const ro = new ResizeObserver(reportHeight);
    ro.observe(panel);
    ro.observe(sessions);
    return () => ro.disconnect();
  }, [reportHeight]);

  // 活跃/历史分层（P2）：活跃区全展示，历史区折叠。M1-10 收口：
  // 活跃区渲染前 ACTIVE_LIMIT 张，历史展开区渲染前 HISTORY_LIMIT 张，
  // 溢出走「查看更多会话」直达会话窗口（轻面板扫一眼，重管理进窗口）。
  // 每次渲染现算（几十个会话开销可忽略），保证 30s 定时刷新时"空闲→已结束"即时翻转
  const sorted = sortSessions(snap.sessions);
  const active = sorted.filter((s) => !displayState(s.state, s.last_activity_at).ended);
  const history = sorted.filter((s) => displayState(s.state, s.last_activity_at).ended);

  // 额度区（M3-6，06 §9.1）：岛展示集实例；「额度」链接在区头常设
  // （批次二：eyebrow 区头统一三段结构——会话/额度各一行小标＋内容）
  const islandAccounts = snap.accounts.filter((a) => a.in_island);
  const quotaLink = (
    <Tip content="打开额度窗口：全部供应商余额与窗口用量">
      <button className="sum-link" onClick={openQuotas}>
        {/* 硬币图标与托盘菜单「额度」项／额度页空态同源（原误用报表柱状图，
            系复制汇总条「报表」按钮 JSX 后忘换图标） */}
        <CoinIcon />
        额度
      </button>
    </Tip>
  );
  // 展示集被清空＝额度区整体隐藏（尊重显式选择）；零实例保留灰字引导＋额度页入口
  const showQuota = snap.accounts.length === 0 || islandAccounts.length > 0;
  return (
    <div className="panel" ref={panelRef}>
      <SummaryBar snap={snap} />
      {/* 会话区头（批次二 eyebrow 结构）：小标＋计数即入口；常驻提示「点击卡片
          跳转对应窗口」收敛进气泡（空面板 0 卡片时不再显示无对象的指引） */}
      <div className="panel-title">
        <Tip content={`进行中 ${active.length} 个 · 共 ${snap.sessions.length} 个（含已结束 ${history.length} 个）\n点击打开会话窗口；点击会话卡片可跳转对应终端/IDE`}>
          <button className="panel-title-link" onClick={openSessions}>
            会话 · {active.length} / {snap.sessions.length}
          </button>
        </Tip>
      </div>
      <div className="panel-sessions" ref={sessionsRef}>
        {active.slice(0, ACTIVE_LIMIT).map((s) => (
          <SessionCard key={s.id} s={s} />
        ))}
        {active.length > ACTIVE_LIMIT && <MoreLink hidden={active.length - ACTIVE_LIMIT} />}
        {history.length > 0 && <HistorySection all={history} />}
        {snap.sessions.length === 0 && (
          <div className="panel-empty">
            未检测到会话
            {/* 「岛 → 设置」一步可达（07-UX 1.1）：新用户首启最可能停在这条
                空态上，给一条不抢眼的文字链直达设置窗 */}
            <button
              type="button"
              className="panel-empty-link"
              onClick={() => invoke("show_settings_window").catch(() => {})}
            >
              打开设置
            </button>
          </div>
        )}
      </div>
      {showQuota && (
        <div
          className="panel-quota"
          onMouseEnter={() => {
            quotaHoverRef.current = true;
          }}
          onMouseLeave={() => {
            // 只清标志位（07-UX 3.8）：移出下一拍恢复，不吃点击切换的 30s 惩罚
            quotaHoverRef.current = false;
          }}
        >
          {/* 密度对齐改版：有实例时标题行（额度＋身份＋轮播点＋链接）由
              QuotaCarousel 渲染——身份与轮播点依赖轮播帧序号状态；
              零实例态无轮播，标题行就地渲染（与零会话空态同一形态） */}
          {islandAccounts.length > 0 ? (
            <QuotaCarousel
              accounts={islandAccounts}
              thresholds={thresholds}
              link={quotaLink}
              hoverRef={quotaHoverRef}
            />
          ) : (
            <>
              <div className="panel-title">
                <span>额度</span>
                {quotaLink}
              </div>
              <div className="panel-empty">尚未配置供应商，到设置页「额度与凭据」添加</div>
            </>
          )}
        </div>
      )}
    </div>
  );
}
