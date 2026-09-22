/**
 * 展开面板（2026-09-18 展示改造 P1-P11；M1-10 收口）：
 * 今日汇总条（活跃/今日消耗/调用次数 + 报表入口）
 * → 会话列表（活跃区/历史展开区各设条数上限，溢出显示「查看更多会话」
 *   链接直达会话窗口；历史区默认折叠为一行摘要，点击展开）
 * → GLM 额度区（双窗口进度条+倒计时）。
 * 状态文案/标题回退/排序口径抽至 shared/sessionDisplay（与会话窗口同源）。
 * 卡片两行：第一行 = 状态·相对时间 + 会话标题（主文案）；
 * 第二行 = 模型/项目徽章 + token（悬浮展示四项拆解）。
 * 点击会话卡片的跳转行为由 T10 接入
 */
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import type { IslandSnapshot, SessionView, Thresholds } from "../shared/types";
import {
  AGENT_DEFS,
  agentColor,
  errorReason,
  fmtCountdownCN,
  fmtRelative,
  fmtTokens,
  quotaLevel,
} from "../shared/types";
import { AGENT_BADGE, cardTitle, displayState, sortHistory, sortSessions } from "../shared/sessionDisplay";
import Tip from "../shared/Tip";
import { ChipIcon, FolderIcon, ReportIcon } from "../shared/icons";

/** 活跃区卡片上限（宽松：会话少时保持一眼全览；超出部分走会话窗口） */
const ACTIVE_LIMIT = 12;

/** 历史区展开后的卡片上限（同样溢出直达会话窗口） */
const HISTORY_LIMIT = 20;

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
  // 点击跳转：激活该会话对应的终端/IDE 窗口（T10；未命中静默失败）
  const focus = () => {
    invoke("focus_session", { sessionId: s.id }).catch(() => {});
  };
  return (
    <div className={`card${disp.ended ? " card-ended" : ""}`} data-session-id={s.id} onClick={focus}>
      <span className={`dot ${disp.dot}`} />
      {/* 徽标底色 = Agent 身份色（颜色即身份） */}
      <span className="card-badge" style={{ background: agentColor(s.agent) }}>
        {AGENT_BADGE[s.agent] ?? "??"}
      </span>
      <div className="card-main">
        <div className="card-line1">
          {/* 悬浮展示完整标题（截断有省略号暗示，气泡只作补充——G2 规则③） */}
          <Tip content={title}>
            <span className="card-title">{title}</span>
          </Tip>
          <span className={`card-state${s.state === "error" ? " text-error" : ""}`}>{stateFinal}</span>
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

  // 按 Agent 聚合已结束计数，条目按计数降序（常用自浮，无字母序沉底问题）
  const counts = new Map<string, number>();
  for (const s of all) counts.set(s.agent, (counts.get(s.agent) ?? 0) + 1);
  const menuAgents = [...counts.entries()].sort((a, b) => b[1] - a[1]);

  const list = sortHistory(agent ? all.filter((s) => s.agent === agent) : all);
  const latest = list[0];
  const latestText = latest
    ? `${cardTitle(latest.title, latest.project_dir, latest.id)} · ${fmtRelative(latest.last_activity_at)}`
    : "";
  const agentLabel = (id: string) =>
    AGENT_DEFS.find((a) => a.id === id)?.label ?? AGENT_BADGE[id] ?? id;
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
                  当前筛选：仅 {agentLabel(agent)}，命中 {list.length} / 共 {all.length} 个
                </div>
              ) : (
                <div className="tip-dim">共 {all.length} 个 · 点击展开 / 收起列表</div>
              )}
            </div>
          }
        >
          <button className="history-toggle" onClick={toggleOpen}>
            <span className={`chevron${open ? " chevron-open" : ""}`}>▸</span>
            已结束 {list.length} 个{agent && <span> · 仅 {agentLabel(agent)}</span>}
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
            {agent ? AGENT_BADGE[agent] ?? agentLabel(agent) : "全部"} ▾
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
                <span className="history-menu-dot" style={{ background: agentColor(id) }} />
                <span className="history-menu-name">{agentLabel(id)}</span>
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
    status = `已用 ${Math.round(pct)}% · 预计 ${fmtCountdownCN(resetAt)}后重置`;
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

/** 今日汇总条（P1）：面板首行回答"今天整体怎么样"，报表按钮直达报表窗口 */
function SummaryBar({ snap }: { snap: IslandSnapshot }) {
  const activeNow = snap.sessions.filter(
    (s) => s.state === "working" || s.state === "waiting" || s.state === "error",
  ).length;
  const openReport = () => {
    invoke("show_report_window").catch(() => {});
  };
  return (
    <div className="sum-bar">
      <Tip content={`今日 ${fmtTokens(snap.today_tokens)}（含缓存，账单口径）`}>
        <span className="sum-text">
          {snap.today_calls > 0 ? (
            <>
              活跃 <b>{activeNow}</b> · 今日 <b>{fmtTokens(snap.today_tokens)}</b> · {snap.today_calls} 次调用
            </>
          ) : (
            "今日暂无用量"
          )}
        </span>
      </Tip>
      <Tip content="打开报表窗口">
        <button className="sum-link" onClick={openReport}>
          <ReportIcon />
          报表
        </button>
      </Tip>
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

  const q5h = snap.quotas.find(
    (q) => q.provider === "glm" && q.window_kind === "5h",
  );
  const qWeek = snap.quotas.find(
    (q) => q.provider === "glm" && q.window_kind === "weekly",
  );
  return (
    <div className="panel" ref={panelRef}>
      <SummaryBar snap={snap} />
      <div className="panel-title">
        {/* 活跃数/总数：活跃区卡片数与历史区「已结束 N 个」之和恰为总数，悬浮对账；
            标题行可点击直达会话窗口（M1-10 入口） */}
        <Tip
          content={`进行中 ${active.length} 个 · 共 ${snap.sessions.length} 个（含已结束 ${history.length} 个）。点击打开会话窗口`}
        >
          <button className="panel-title-link" onClick={openSessions}>
            会话 · {active.length} / {snap.sessions.length}
          </button>
        </Tip>
        <span className="panel-hint">点击卡片跳转对应窗口</span>
      </div>
      <div className="panel-sessions" ref={sessionsRef}>
        {active.slice(0, ACTIVE_LIMIT).map((s) => (
          <SessionCard key={s.id} s={s} />
        ))}
        {active.length > ACTIVE_LIMIT && <MoreLink hidden={active.length - ACTIVE_LIMIT} />}
        {history.length > 0 && <HistorySection all={history} />}
        {snap.sessions.length === 0 && (
          <div className="panel-empty">暂无会话记录</div>
        )}
      </div>
      <div className="panel-quota">
        <div className="panel-title">GLM Coding Plan</div>
        <div className="quota-row">
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
      </div>
    </div>
  );
}
