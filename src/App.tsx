/**
 * 应用入口：按窗口 URL hash 分流 —— #settings 渲染设置页（常规窗口），
 * #report 渲染报表页（常规窗口），#quotas 渲染额度页（常规窗口，M3-5），
 * #about 渲染关于页（常规窗口），
 * 其余渲染灵动岛（透明窗口）；岛消费 island-snapshot 快照，hover 展开面板
 */
import { lazy, Suspense, useEffect, useMemo, useRef, useState } from "react";
import { listen, emitTo } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { syncDisplayConstants } from "./shared/sessionDisplay";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { LogicalSize } from "@tauri-apps/api/dpi";
import IslandBar from "./island/IslandBar";
import Panel from "./island/Panel";
import EdgeTab from "./island/EdgeTab";
// 设置页 lazy 分割（四轮审查补漏）：与其余五窗同款——独立窗口独立加载，
// 岛窗口 bundle 不含设置页代码
const Settings = lazy(() => import("./Settings"));
import { AGENT_DEFS, setAgentColors } from "./shared/types";
import {
  parseAgentLimit,
  parseAgentOrder,
  setAgentLimit,
  setAgentOrder,
} from "./shared/agentOrder";
import { useTheme } from "./shared/theme";
import {
  ISLAND_OPACITY_EVENT,
  applyIslandOpacity,
  asIslandOpacity,
} from "./shared/islandOpacity";
import { DEFAULT_THRESHOLDS, sanitizeThresholds } from "./shared/types";
import type { IslandSnapshot, Thresholds } from "./shared/types";
import { buildTooltipPayload } from "./shared/islandTooltip";
import { ErrorBoundary } from "./shared/ErrorBoundary";
import "./App.css";

// 报表页 lazy 分割：echarts 只在报表窗口加载，岛窗口 bundle 不受影响（01-RESEARCH §9）
const Report = lazy(() => import("./report/Report"));
// 额度页 lazy 分割（M3-5）：与报表页同款——独立窗口独立加载，岛窗口 bundle 不受影响
const Quotas = lazy(() => import("./quotas/Quotas"));
// 会话窗口 lazy 分割（M1-10）：与报表页同款——独立窗口独立加载，
// 不含 echarts；岛窗口 bundle 不受影响
const Sessions = lazy(() => import("./sessions/Sessions"));
// 关于页 lazy 分割：与报表页同款——独立窗口独立加载，岛窗口 bundle 不受影响
const About = lazy(() => import("./about/About"));
// 托盘菜单页 lazy 分割：托盘右键弹出的自绘菜单（M1-9），独立窗口独立加载
const TrayMenu = lazy(() => import("./traymenu/TrayMenu"));
// 岛悬停气泡窗口 lazy 分割（第七轮 4.1）：替代原生 title 的结构化品牌气泡，
// 独立窗口独立加载（岛窗口 bundle 只多一份 emit 组装逻辑，不含气泡渲染）
const TooltipWindow = lazy(() => import("./island/TooltipWindow"));

/** 岛自适应尺寸（Rust island_metrics：显示器逻辑宽 × 30%，夹取 380～800） */
interface IslandMetrics {
  width: number;
  collapsed_h: number;
  expanded_h: number;
  /** 收纳形态（横线/小岛）宽度（mini_line_width：大岛宽×40% 夹取 [240， 320]） */
  mini_line_w: number;
  /** 工作区底缘（逻辑 y）：展开封顶用（五轮审查批次三）——岛窗口左上角锚定、
   *  面板向下生长，岛在屏幕下半部时须按「底缘 − 岛 y」限制展开高度 */
  work_bottom: number;
}
const DEFAULT_METRICS: IslandMetrics = {
  // 380 与 tauri.conf 的岛窗出生宽一致（五轮审查：原 360 会误入 w-sm 档一帧）
  width: 380,
  collapsed_h: 48,
  expanded_h: 520,
  // 200 与 Rust MINI_W_MIN 同源（07 §7：DEFAULT_METRICS 必须与 Rust 常量同步，防首帧闪错）
  mini_line_w: 200,
  work_bottom: 672,
};

/** 面板与胶囊的间距（与 App.css .panel-wrap 的 margin-top 保持一致） */
const PANEL_GAP_PX = 6;
/** 滑出隐藏的渲染切换延迟（#3「延迟切换」）：与 Rust SLIDE_HIDE_MS（160ms）
 *  同源双端维护＋10ms 裕量——等窗口滑出动画播完再挂贴边信标，期间胶囊随窗
 *  自然退场，信标不再从滑出起点"横穿"到落位 */
const EDGE_SWAP_DELAY_MS = 170;
/** 展开态窗口高度下限：空会话时骨架（汇总条+标题+空态+额度区）仍完整可用的最小高度 */
const MIN_EXPANDED_H = 240;

// 阈值默认值与清洗已收敛至 shared/types（四轮审查：原先 App/Quotas/TrayMenu
// 三处各写一份 80/95/10/5 与逐项校验，口径有漂移风险）——R5：设置页可配，
// 启动时加载一次，重启生效

/** 岛的渲染相位（07-TOP-ISLAND §4）：mini＝小岛常驻态；full＝大岛；panel＝面板展开。
 *  一粒岛的三次生长——形态切换全在 CSS（clip-path 形变），本相位只驱动类名/挂载/上报 */
type IslePhase = "mini" | "full" | "panel";

/** 静止相位（hover 离开后回到的常态，07 §4.2）：左右停靠不隐藏＝大岛常驻（现状
 *  胶囊停靠）；自由位/贴顶（含隐藏态恢复）＝小岛。位置只决定停靠几何与静止形态，
 *  不决定信息层级 */
function restPhase(dock: { edge: string; hidden: boolean }): IslePhase {
  if (!dock.hidden && (dock.edge === "left" || dock.edge === "right")) return "full";
  return "mini";
}

/** island_metrics 返回值归一（07 §7 防御）：后端旧版本缺 mini_line_w 字段时
 *  回落默认——严禁 undefined 流进 --isle-w/width（非法 var 会让整条 clip-path
 *  失效、width undefined 回落 100%，2026-10-12 实机"小岛全宽/横线全宽"事故根因）。
 *  bootstrap 与 island-dock 事件刷新两条路径共用，防一边修一边漏 */
function normalizeMetrics(m: Partial<IslandMetrics> | null): IslandMetrics {
  if (!m) return DEFAULT_METRICS;
  return {
    width: m.width ?? DEFAULT_METRICS.width,
    collapsed_h: m.collapsed_h ?? DEFAULT_METRICS.collapsed_h,
    expanded_h: m.expanded_h ?? DEFAULT_METRICS.expanded_h,
    mini_line_w: m.mini_line_w ?? DEFAULT_METRICS.mini_line_w,
    work_bottom: m.work_bottom ?? DEFAULT_METRICS.work_bottom,
  };
}

function IslandApp() {
  // 主题应用与跟随（M1-4）：结果写 <html> 的 data-theme，CSS 变量自动切换；岛无需感知返回值
  useTheme();
  const [snap, setSnap] = useState<IslandSnapshot | null>(null);
  // 岛相位（2026-10-12 改版）：原 expanded:boolean 扩展为三态，mini＝默认
  const [phase, setPhase] = useState<IslePhase>("mini");
  // 面板动画相位：in=挂载且入场中/展开，out=退场动画在播，null=未挂载。
  // 退场必须等动画播完才卸载并缩窗——窗口先缩会拦腰截断动画
  const [panelPhase, setPanelPhase] = useState<"in" | "out" | null>(null);
  // 面板焦点归还（2026-10-16 审查）：挂载时记录当时焦点元素，卸载（收起/贴边
  // 滑出）后归还——焦点若停留在面板内会话卡/链接上，面板卸载后掉到 body、
  // Tab 序从头再来（useFocusTrap 的归焦同哲学；岛本体已隐藏时 focus 静默无效，
  // 焦点自然落 body 兜底）
  const panelPrevFocus = useRef<Element | null>(null);
  useEffect(() => {
    if (panelPhase != null) {
      if (panelPrevFocus.current == null) {
        panelPrevFocus.current = document.activeElement;
      }
    } else if (panelPrevFocus.current != null) {
      const el = panelPrevFocus.current;
      panelPrevFocus.current = null;
      if (el instanceof HTMLElement) el.focus();
    }
  }, [panelPhase]);
  // 面板内容自然高度（Panel 上报）：展开高度自适应的依据，null = 未测得
  const [panelH, setPanelH] = useState<number | null>(null);
  // 胶囊"由远及近"入场：隐藏态滑回显示的瞬间置为贴边边（决定动画 origin 与位移方向）。
  // 仅悬停路径消费（标签→胶囊翻转可见）；托盘召唤走物理滑入，无翻转不触发——
  // 召唤时若重播动画会出现"胶囊先出现又闪回动画起点"的跳变（2026-09-20 实测）
  const [peekEnter, setPeekEnter] = useState<string | null>(null);
  const prevHiddenRef = useRef(false);
  // 上次实际下发的窗口尺寸（防循环护栏：观察器→setSize→resize→观察器）。
  // 双值比较（2026-10-03 审查修复）：显示器/缩放变化会改宽度——原先只比高度，
  // 宽度变化但高度相同时 set_size 会被护栏拦掉不发
  const lastSetSize = useRef<{ w: number; h: number } | null>(null);
  const [thresholds, setThresholds] = useState<Thresholds>(DEFAULT_THRESHOLDS);
  // 贴边状态（Rust 端 island-dock 事件推送）：edge=none/top/left/right,hidden=是否滑出隐藏
  const [dock, setDock] = useState<{ edge: string; hidden: boolean }>({
    edge: "none",
    hidden: false,
  });
  const dockRef = useRef(dock);
  // 岛自适应尺寸（挂载时从 Rust 获取，与贴边几何共用同一公式）
  const [metrics, setMetrics] = useState<IslandMetrics>(DEFAULT_METRICS);
  // 悬停是否自动展开信息卡片（设置项 hover_expand；关闭时点击岛展开/收回）
  const [hoverCard, setHoverCard] = useState(true);
  // 监控的 Agent 列表（设置项 agents_enabled；岛内只展示所选 Agent）
  const [agents, setAgents] = useState<string[]>(AGENT_DEFS.map((a) => a.id));
  // 贴边隐藏态下"滑入完成后再展开"的定时器（第一级停留：160ms 形变/240ms 面板链）
  const enterTimer = useRef<number | undefined>(undefined);
  // 第二级停留定时器（大岛→面板：光标入岛累计 480ms / 左右停靠 320ms）
  const panelTimer = useRef<number | undefined>(undefined);
  // 移出后滑出隐藏的宽限定时器（400ms 内回来则取消，防误触）
  const leaveTimer = useRef<number | undefined>(undefined);
  // 面板收起宽限定时器（五轮审查 P1 修复）：见根节点 onMouseLeave 注释
  const collapseTimer = useRef<number | undefined>(undefined);
  // 托盘"显示"召唤后的自动收起定时器（3s；鼠标移入即取消——用户正在看/用岛）
  const summonTimer = useRef<number | undefined>(undefined);
  // 滑出隐藏的渲染切换定时器（#3 延迟切换，见 dock effect 注释）
  const edgeSwapTimer = useRef<number | undefined>(undefined);
  // 滑出动画进行中的渲染保持标记：true＝dock.hidden 已真但继续渲染胶囊
  const [edgeDelay, setEdgeDelay] = useState(false);

  // 启动引导：读取岛尺寸/提醒阈值/悬停展开开关/贴边状态。
  // 初始贴边状态必须主动拉取：启动恢复在 setup 阶段已把窗口滑出隐藏，
  // 早于本窗口事件监听建立，island-dock 事件收不到；不拉取会把隐藏态渲染成完整胶囊。
  // 窗口极早期加载（暖缓存下亚秒级完成）时，挂载瞬间的 invoke 会被无声丢弃且
  // promise 永不落定（2026-09-17 实测），故每 1s 重试直至三项各成功一次；
  // 成功后停表，之后贴边状态改由 island-dock 事件流维护
  useEffect(() => {
    const done = { metrics: false, dock: false, settings: false, setMode: false };
    // 展示常量同步（#20）：「已结束」阈值从后端拉取（Rust 单真值源），失败保持默认
    void syncDisplayConstants();
    const bootstrap = async () => {
      if (!done.metrics) {
        try {
          const m = await invoke<IslandMetrics>("island_metrics");
          if (m) setMetrics(normalizeMetrics(m)); // 字段兜底归一（见 normalizeMetrics 注释）
          done.metrics = true;
        } catch {
          /* 通道未就绪，下轮重试 */
        }
      }
      if (!done.dock) {
        try {
          const d = await invoke<{ edge: string; hidden: boolean }>("island_dock_state");
          dockRef.current = d;
          setDock(d);
          // prevHiddenRef 同步推进（B29：原 [dock] effect 已删，监听器只覆盖
          // 事件流——启动恢复早于监听建立，此处不推进则首次滑入的入场
          // 动画参数（peekEnter）不触发）
          prevHiddenRef.current = d.hidden;
          done.dock = true;
        } catch {
          /* 通道未就绪，下轮重试 */
        }
      }
      if (!done.setMode) {
        // 形态上报首达保障（2026-10-13 B16）：挂载期 invoke 可能无声悬挂
        // （永不落定，2026-09-17 实测先例），phase 不变则 phase effect 永不
        // 重发＝穿透调停失去 mini_mode 输入。race 超时防悬挂卡死 bootstrap；
        // 首达必为 mini（启动默认态，先于任何 hover）
        try {
          await Promise.race([
            invoke("island_set_mode", { mode: "mini" }),
            new Promise((_, rej) =>
              window.setTimeout(() => rej(new Error("setMode timeout")), 1500),
            ),
          ]);
          done.setMode = true;
        } catch {
          /* 悬挂/失败：下轮重试 */
        }
      }
      if (!done.settings) {
        try {
          const s = await invoke<Record<string, string>>("get_settings");
          setThresholds(sanitizeThresholds(s));
          if (s.hover_expand !== undefined) setHoverCard(s.hover_expand !== "0");
          if (s.agents_enabled) {
            try {
              const list = JSON.parse(s.agents_enabled) as string[];
              if (Array.isArray(list)) setAgents(list); // 空数组 = 用户选择全部不监控，尊重之
            } catch {
              /* 解析失败用默认全选 */
            }
          }
          if (s.agent_colors) {
            try {
              const colors = JSON.parse(s.agent_colors) as Record<string, string>;
              if (colors && typeof colors === "object") setAgentColors(colors);
            } catch {
              /* 解析失败用默认色 */
            }
          }
          // Agent 展示顺序（2026-10-03 拖拽排序）：注入共享注册表，面板筛选
          // 菜单/贴边分段条直接消费（键缺失＝默认序）
          setAgentOrder(parseAgentOrder(s.agents_order));
          // 上岛深度（2026-10-08「前 N 个」）：贴边分段过滤消费（键缺失＝默认 5）
          setAgentLimit(parseAgentLimit(s.island_agent_limit));
          // 背景不透明度：搭 bootstrap 重试便车——极早期 invoke 永不落定时靠 1s 重试兜住
          applyIslandOpacity(asIslandOpacity(s.island_opacity));
          done.settings = true;
        } catch {
          /* 通道未就绪，下轮重试 */
        }
      }
    };
    void bootstrap();
    // 重试上限（2026-10-03 审查对齐 invokeReady 哲学）：真故障不无限静默空转
    // （60 轮＝1 分钟，足够覆盖启动竞态；后端真死时窗口随主进程消亡）
    let tries = 0;
    const timer = window.setInterval(() => {
      if (done.metrics && done.dock && done.settings && done.setMode) {
        window.clearInterval(timer);
        return;
      }
      if (tries >= 60) {
        window.clearInterval(timer);
        return;
      }
      tries++;
      void bootstrap();
    }, 1000);
    return () => window.clearInterval(timer);
  }, []);

  // 设置页修改悬停展开开关后实时推送
  useEffect(() => {
    const un = listen<boolean>("hover-expand-changed", (e) =>
      setHoverCard(e.payload),
    );
    return () => {
      un.then((f) => f());
    };
  }, []);

  // 设置页修改监控 Agent 列表/颜色后实时推送；order/limit 为可选字段——
  // order 仅拖拽重排的 emit 携带、limit 仅上岛深度调整的 emit 携带（2026-10-08），
  // 读不到时各自维持原值
  useEffect(() => {
    const un = listen<{
      agents: string[];
      colors: Record<string, string>;
      order?: string[];
      limit?: number;
    }>("agents-changed", (e) => {
      setAgents(e.payload.agents);
      setAgentColors(e.payload.colors);
      if (Array.isArray(e.payload.order)) setAgentOrder(e.payload.order);
      if (typeof e.payload.limit === "number") setAgentLimit(e.payload.limit);
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  // 设置页修改额度阈值后实时推送（2026-09-29 分组优化）：水位分色即时跟随，
  // 不再要求重开窗口。2026-10-03 审查修复：载荷扩为四字段（百分比＋两条余额
  // 警戒线，原先余额线不广播岛端读到重启前一直是旧值），按字段各自校验合并
  useEffect(() => {
    const un = listen<{
      warn?: number;
      danger?: number;
      balanceWarn?: number;
      balanceWarnUsd?: number;
    }>("thresholds-changed", (e) => {
      const p = e.payload ?? {};
      const fin = (v: number | undefined): v is number =>
        v != null && Number.isFinite(v);
      setThresholds((t) => ({
        ...t,
        // 百分比段维持原交叉校验（两值齐全且 warn < danger 才合并）
        ...(fin(p.warn) && fin(p.danger) && p.warn > 0 && p.danger <= 100 && p.warn < p.danger
          ? { warn: p.warn, danger: p.danger }
          : {}),
        // 余额线：非负即合并（单档改动时其余字段照发全量）
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

  // 设置页拖动「背景不透明度」滑块后实时推送 → 重新派生三层 alpha
  useEffect(() => {
    const un = listen<number>(ISLAND_OPACITY_EVENT, (e) =>
      applyIslandOpacity(asIslandOpacity(e.payload)),
    );
    return () => {
      un.then((f) => f());
    };
  }, []);

  // 订阅贴边状态（吸附/滑出/滑入时由 Rust 推送）。⚠ B28：隐藏事件的渲染冻结
  // （edgeDelay）与 panelPhase 复位必须在监听器内与 setDock **同批 setState**——
  // B25 曾经 effect([dock.hidden]) 补设，而 useEffect 晚于首帧绘制：dock.hidden=true
  // 的第一帧以 edgeDelay=false 渲染＝EdgeTab 闪现一帧、IslandBar 被卸载重挂
  // （重挂元素 clip-path 无过渡起点＝收缩动画被拦腰打断、「大岛瞬灭→小岛
  // 闪现」跳变，2026-10-13 实机取证）。同批 setState 一次渲染到位，零中间帧；
  // 启动引导的 island_dock_state 直拉不经此逻辑（恢复隐藏时窗口本已在隐藏位，
  // 无滑出动画可衔接）
  useEffect(() => {
    const un = listen<{ edge: string; hidden: boolean }>("island-dock", (e) => {
      dockRef.current = e.payload;
      if (e.payload.hidden) {
        // #3 延迟切换（B25）：滑出动画期间继续渲染胶囊随窗退场，播完再挂
        // 信标；panelPhase 立即复位（B21 不变量：隐藏卸载路径动画回调不可达，
        // 不复位＝卡死 "out" 劫持后续滑入）；setPhase(mini) 推迟到切换回调——
        // 期间保持 full 让整条胶囊退场（穿透等式只看 hidden，mini_mode 晚到
        // 无实害）；期间回悬停：hoverEnter 走隐藏分支调 island_peek(true) →
        // hidden=false 事件到达下方 else 分支撤销定时器，全程事件驱动
        setPanelPhase(null);
        setEdgeDelay(true);
        window.clearTimeout(edgeSwapTimer.current);
        edgeSwapTimer.current = window.setTimeout(() => {
          setEdgeDelay(false);
          setPhase((p) => (p === "mini" ? p : "mini"));
        }, EDGE_SWAP_DELAY_MS);
      } else {
        // 重新显示（回悬停/托盘召回/吸附落位/Gone 预切）：撤销待切换定时器，
        // 恢复即时渲染。B29 三连补（均须与 setDock 同批，理由同上）：
        // ① peekEnter 同批注入——经 effect 补设晚一帧＝IslandBar 首帧无
        //    island-peek-in 类（全不透明落帧）再补类重播＝启动后首次滑入
        //    边缘碎片闪现（B28 同族）；
        // ② panelPhase 一并复位——Gone（托盘隐藏）预切事件只发 hidden:false，
        //    面板开着被整窗隐藏时退场动画 onAnimationEnd 永不触发（B21 族
        //    盲区）＝panelPhase 卡 "out"；且 Rust 侧已把窗口压回 48px、前端
        //    lastSetSize 缓存仍是高值＝下次召唤面板被裁剪，在此断源；
        // ③ prevHiddenRef 随事件推进（原 [dock] effect 已删；启动直拉路径
        //    在 bootstrap 内自行更新）
        window.clearTimeout(edgeSwapTimer.current);
        setEdgeDelay(false);
        setPanelPhase(null);
        if (prevHiddenRef.current && e.payload.edge !== "none") {
          setPeekEnter(e.payload.edge);
        }
      }
      prevHiddenRef.current = e.payload.hidden;
      setDock(e.payload);
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  // 隐藏 → 显示的入场动画参数（peekEnter）已随 island-dock 监听器同批注入
  //（B29，原 [dock] effect 补设晚一帧＝首次滑入闪现，见监听器注释）

  // phase=panel 翻转映射到面板动画相位：展开即挂载入场；收起先进退场动画，播完自动卸载
  useEffect(() => {
    if (phase === "panel") setPanelPhase("in");
    else setPanelPhase((p) => (p === "in" ? "out" : p));
  }, [phase]);

  // 形态上报（07 §4.4 穿透契约）：mini 态窗口两侧需穿透放行；full/panel 恒不穿透。
  // 相位驱动——mouseleave 的 150ms 宽限到期才回到 mini（收缩期宁拦截，可自愈）；
  // 面板退场动画在播时 phase 已非 panel，但面板正在消失、不可交互，穿透无实害
  useEffect(() => {
    invoke("island_set_mode", { mode: phase === "mini" ? "mini" : "expanded" }).catch(() => {});
  }, [phase]);

  // 托盘"显示"召唤（island-summon）：贴边停靠的岛滑回后 3s 自动滑出收回——
  // 召唤是"临时亮位提醒"，无人理会就自己收好；鼠标移入则取消（交给常规
  // 移出滑出逻辑接管），期间被托盘隐藏或拖走也不动作
  useEffect(() => {
    const un = listen("island-summon", () => {
      // 召唤的入场由 Rust 物理滑入承担（island_transition Pill），此处只负责
      // 3s 自动收回计时；不重播 CSS 入场动画（避免胶囊已亮出又闪回动画起点）
      window.clearTimeout(summonTimer.current);
      // 召唤形态（B29）：贴边用户按 07 §4.2 应「滑入大岛亮出」——此前 phase
      // 沿用滑出时残留的 mini（召唤无 hover 事件驱动形态）＝召唤出小岛，与
      // 规格不符；显式升 full（自由位 edge=none 不动，保持原地常驻语义）
      if (dockRef.current.edge !== "none") setPhase("full");
      summonTimer.current = window.setTimeout(() => {
        if (
          dockRef.current.edge !== "none" &&
          !dockRef.current.hidden &&
          document.visibilityState === "visible"
        ) {
          invoke("island_peek", { show: false }).catch(() => {});
        }
      }, 3000);
    });
    return () => {
      window.clearTimeout(summonTimer.current);
      un.then((f) => f());
    };
  }, []);

  // 启动完成信号（2026-09-24 启动居中展示）：首个数据快照到达即通知 Rust 把
  // 启动态居中展示的胶囊滑回记忆位（上次退出位置）；只发一次，运行期失效为
  // no-op，invoke 失败不致命（居中位继续用，无功能损失）
  const bootSettled = useRef(false);
  useEffect(() => {
    const unlisten = listen<IslandSnapshot>("island-snapshot", (e) => {
      setSnap(e.payload);
      if (!bootSettled.current) {
        bootSettled.current = true;
        invoke("island_boot_settled").catch(() => {});
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // 展开/收起：仅调高度；失败不致命（尺寸权限缺失时内容被裁剪但不崩溃）。
  // 展开高度按面板内容自适应（2026-09-18 展示改造）：
  //   目标 = 胶囊 + 间距 + 面板自然高度，上限 expanded_h（超出面板内部滚动），
  //   下限 MIN_EXPANDED_H；未测得前回退 expanded_h（与历史行为一致，避免闪缩）。
  // 屏幕下缘封顶（五轮审查批次三）：岛在屏幕下半部时上限改取
  //   min(expanded_h, 工作区底缘 − 岛 y)，防面板伸出屏幕外（内容出屏滚不到）。
  // 收起时序（2026-09-20 动效改造）：面板退场动画在播（phase=out）时窗口保持
  // 原高不动，动画播完卸载（phase=null）后才收缩——提前缩窗会拦腰截断动画。
  // 窗口必须跟随内容收缩：岛常驻顶层，若只缩内容不缩窗口，
  // 下方透明区域会拦截鼠标、挡住下层应用点击

  // 展开态「屏幕下方可用高度」（五轮审查批次三）：工作区底缘 − 岛窗口当前 y。
  // 展开时取一次（两次轻量 IPC，面板 220ms 入场动画足以掩盖先按 520 展开、
  // 数毫秒后收紧的过渡）；收起不重置上次值——仅展开路径消费，避免每次展开
  // 先按 520 闪一拍。取不到（IPC 失败）保持 null＝退回 520 常量行为
  const [availBelow, setAvailBelow] = useState<number | null>(null);
  useEffect(() => {
    if (phase !== "panel") return;
    let alive = true;
    const win = getCurrentWebviewWindow();
    void (async () => {
      try {
        const [pos, scale] = await Promise.all([win.outerPosition(), win.scaleFactor()]);
        if (!alive) return;
        const y = Math.round(pos.y / scale);
        const avail = metrics.work_bottom - y - 8; // 底缘留 8px 呼吸
        setAvailBelow(avail > 0 ? avail : null);
      } catch {
        /* 保持上次值/兜底常量 */
      }
    })();
    return () => {
      alive = false;
    };
  }, [phase, metrics]);

  // 展开态窗口高度上限：屏幕常量与「下方可用高度」取小；与 MIN_EXPANDED_H
  // 冲突时保下限（宁可贴底微溢出，不出畸形窄条）
  const expandedCap = Math.max(
    MIN_EXPANDED_H,
    Math.min(metrics.expanded_h, availBelow ?? metrics.expanded_h),
  );

  useEffect(() => {
    const panelOpen = phase === "panel";
    if (panelPhase == null && panelH != null) setPanelH(null); // 面板卸载后清测量值，下次展开重新上报
    if (!panelOpen && panelPhase != null) return; // 退场动画在播：窗口高度按兵不动
    const win = getCurrentWebviewWindow();
    const target = {
      w: metrics.width,
      h: panelOpen
        ? Math.min(
            expandedCap,
            Math.max(
              MIN_EXPANDED_H,
              metrics.collapsed_h + PANEL_GAP_PX + (panelH ?? metrics.expanded_h),
            ),
          )
        : metrics.collapsed_h,
    };
    // 目标不变不重下发，断开反馈环（宽高双值比较，见 lastSetSize 注释）
    if (lastSetSize.current?.w === target.w && lastSetSize.current?.h === target.h)
      return;
    lastSetSize.current = target;
    win
      .setSize(new LogicalSize(target.w, target.h))
      .catch(() => {});
  }, [phase, metrics, panelH, panelPhase, expandedCap]);

  // 显示器/缩放变化后重拉岛尺寸（2026-10-03 审查修复）：metrics 原先仅挂载
  // 拉取一次，拖岛跨屏或系统缩放变化后宽度档位/标签宽仍用旧值；贴边吸附
  // （island-dock）是拖动结束的天然检查点，顺带重拉
  useEffect(() => {
    const refresh = () => {
      invoke<IslandMetrics>("island_metrics")
        .then((m) => {
          // 字段兜底归一（与 bootstrap 共用）：island-dock 事件（吸附/滑入滑出）
          // 都会触发本刷新，旧后端缺字段时 undefined 会从这里回流污染 metrics
          if (m) setMetrics(normalizeMetrics(m));
        })
        .catch(() => {});
    };
    const un1 = getCurrentWebviewWindow().onScaleChanged(() => refresh());
    const un2 = listen("island-dock", () => refresh());
    return () => {
      un1.then((f) => f());
      un2.then((f) => f());
    };
  }, []);

  // useMemo 稳定引用（2026-10-03 审查优化）：此前每渲染新建 {...snap} 对象，
  // expanded/dock 等局部 state 变化也会让全树拿到新引用，阻断未来的 memo 化
  const visibleSnap = useMemo(() => filterSnap(snap, agents), [snap, agents]);

  // 宽度降级阶梯（F7，2026-09-28 视觉改版批次一）：按岛宽注根类，
  // CSS 据此收窄胶囊段落（≥560 全量 / 460~559 隐今日 / <460 再隐窗口类厂商短名）；
  // metrics 未就绪的默认 380 归 w-md（兜底形态，就绪后由事件刷新）
  const widthClass =
    metrics.width >= 560 ? "w-lg" : metrics.width >= 460 ? "w-md" : "w-sm";

  // 点击直达面板（07 §4.2 强意图通道）：任何形态点击＝跳过停留等待直达面板；
  // 面板开着时再点＝收回到当前静止形态（hover_expand 开关不影响本通道）
  const togglePanel = () =>
    setPhase((p) => (p === "panel" ? restPhase(dockRef.current) : "panel"));

  // ===== 悬停引擎（2026-10-13 B20 定稿）=====
  // 唯一真源＝Rust island-hover-on/off 事件（光标在窗口矩形内 ⇔ 悬停，90ms
  // 轮询沿检测）；DOM mouseenter/mouseleave 完全弃用——穿透翻转的合成事件
  // 污染无法根除（B18/B19 实锤），弃用即免疫。onMouseDown（拖拽）与点击保留。
  // 收缩冷却（B18）：收缩后 ≤90ms 内穿透翻转可能再合成一次 on 事件，250ms
  // 冷却吸收之
  const lastCollapseRef = useRef(0);
  // 相位现读 ref：气泡到期回调在 500ms 后运行，闭包捕获的渲染值已过期——
  // 面板是否已展开必须按到期时刻的真值判定
  const phaseRef = useRef<IslePhase>("mini");
  phaseRef.current = phase;

  // ===== 悬停气泡（第七轮 4.1 品牌化重做，替代原生 title）=====
  // 悬停到期 500ms 出泡（Tip 同款防误触闸门）；独立 tooltip 窗口渲染，
  // 全程只 emit 事件，不触碰岛窗口自身几何/穿透（B20 定稿区零侵入）
  const tooltipTimer = useRef<number | undefined>(undefined);
  // 气泡显示中标记：收泡事件只在发出过 show 后才发（hide 幂等但省无谓 IPC）
  const tooltipShownRef = useRef(false);

  /** 收泡：清到期定时器＋通知 tooltip 窗口隐藏（未显示过则只清定时器） */
  const hideIslandTooltip = () => {
    window.clearTimeout(tooltipTimer.current);
    if (tooltipShownRef.current) {
      tooltipShownRef.current = false;
      emitTo("tooltip", "island-tooltip", { action: "hide" }).catch(() => {});
    }
  };

  const hoverEnter = () => {
    if (performance.now() - lastCollapseRef.current < 250) return;
    window.clearTimeout(leaveTimer.current);
    window.clearTimeout(collapseTimer.current);
    window.clearTimeout(enterTimer.current);
    window.clearTimeout(panelTimer.current);
    // 用户正在看/用岛：托盘召唤的自动收起作废，收起交给移出逻辑
    window.clearTimeout(summonTimer.current);
    // 悬停重入＝重新计时（气泡若在显示中保持不动，锚定快照不闪变）
    window.clearTimeout(tooltipTimer.current);
    tooltipTimer.current = window.setTimeout(() => {
      // 到期条件运行时判定：面板已展开／贴边隐藏态（悬停即滑入，无稳定展示
      // 时机，EdgeTab 头注约定不变）不出泡；滑入完成后 hidden 已翻 false 可出泡
      if (phaseRef.current === "panel" || dockRef.current.hidden) return;
      // 已显示不重发（重入重新计时后到期的重复回调）：内容锚定不闪变
      if (tooltipShownRef.current) return;
      const payload = buildTooltipPayload(visibleSnap, thresholds);
      if (!payload) return;
      tooltipShownRef.current = true;
      emitTo("tooltip", "island-tooltip", { action: "show", data: payload }).catch(
        () => {},
      );
    }, 500);
    // 面板退场未完即返回：直接续看，不闪收闪开（收起是 150ms 宽限的推测，
    // 人回来了就推翻它）。⚠ 仅非隐藏态（B21）：隐藏态必须优先滑入分支——
    // 续看分支的 return 会吞掉 island_peek
    if (panelPhase === "out" && !dockRef.current.hidden) {
      setPhase("panel");
      return;
    }
    if (dockRef.current.hidden) {
      // 隐藏态入口（横线/左右端帽，07 §4.2）：强意图——滑入直达大岛（前端立即
      // 渲染 full，物理滑入由 Rust 承担），累计 480ms 后面板
      invoke("island_peek", { show: true }).catch(() => {});
      setPhase("full");
      if (hoverCard) {
        panelTimer.current = window.setTimeout(() => setPhase("panel"), 480);
      }
    } else if (dockRef.current.edge === "left" || dockRef.current.edge === "right") {
      // 左右停靠态：大岛常驻，320ms 后面板（D6）
      if (hoverCard) {
        enterTimer.current = window.setTimeout(() => setPhase("panel"), 320);
      }
    } else {
      // 小岛常态：160ms 停留形变大岛（防误触闸门），累计 480ms 面板
      enterTimer.current = window.setTimeout(() => setPhase("full"), 160);
      if (hoverCard) {
        panelTimer.current = window.setTimeout(() => setPhase("panel"), 480);
      }
    }
  };
  const hoverEnterRef = useRef(hoverEnter);
  hoverEnterRef.current = hoverEnter;

  /** 悬停离开（island-hover-off 驱动）：150ms 宽限后收缩（宽限期内 on 再来
   *  即取消——Rust 光标真值的对冲，无需 DOM :hover 探测）；贴边停靠另排
   *  400ms 后滑出隐藏 */
  const hoverLeave = () => {
    window.clearTimeout(enterTimer.current);
    window.clearTimeout(panelTimer.current);
    window.clearTimeout(collapseTimer.current);
    // 悬停已走：气泡立即收（原生 title 同款即时性，不等 150ms 宽限）
    hideIslandTooltip();
    collapseTimer.current = window.setTimeout(() => {
      lastCollapseRef.current = performance.now();
      setPhase(restPhase(dockRef.current));
    }, 150);
    if (dockRef.current.edge !== "none") {
      // 400ms 宽限后滑出隐藏（期间重新进入则取消）
      window.clearTimeout(leaveTimer.current);
      leaveTimer.current = window.setTimeout(() => {
        invoke("island_peek", { show: false }).catch(() => {});
      }, 400);
    }
  };
  const hoverLeaveRef = useRef(hoverLeave);
  hoverLeaveRef.current = hoverLeave;

  // 悬停真源监听（挂载即建，ref 保新鲜闭包）
  useEffect(() => {
    const unOn = listen("island-hover-on", () => hoverEnterRef.current());
    const unOff = listen("island-hover-off", () => hoverLeaveRef.current());
    return () => {
      unOn.then((f) => f());
      unOff.then((f) => f());
    };
  }, []);

  // 气泡收泡联动（第七轮 4.1）：进入面板即收（面板已承载全部深化信息）；
  // 贴边滑出隐藏时收（窗口将不可见，气泡失去锚点）。两处只发事件不动岛自身
  // 几何/穿透（B20 定稿区零侵入）
  useEffect(() => {
    if (phase === "panel") hideIslandTooltip();
  }, [phase]);
  useEffect(() => {
    if (dock.hidden) hideIslandTooltip();
  }, [dock.hidden]);
  // 卸载清理（对齐既有 timer 纪律）：在播的出泡定时器不泄漏
  useEffect(() => () => window.clearTimeout(tooltipTimer.current), []);

  // 右键菜单堵漏（2026-10-13 三轮审查 N1）：岛窗口全只读、无文本输入场景，
  // 禁用 WebView2 默认右键菜单——误触「刷新」会重载整个岛 webview、重走启动
  // 引导；仅岛窗口生效（本组件只在岛窗口渲染），设置/会话等常规窗口保留
  // 原生菜单（输入框右键粘贴等）
  useEffect(() => {
    const stop = (e: Event) => e.preventDefault();
    window.addEventListener("contextmenu", stop);
    return () => window.removeEventListener("contextmenu", stop);
  }, []);

  return (
    <div
      className={`root ${widthClass}`}
      onMouseDown={(e) => {
        // 拖拽开始：取消在播滑动动画，防止程序化移动打断系统拖拽。
        // 仅左键触发（2026-10-16 审查）：右键点岛不该打断在播动画
        if (e.button !== 0) return;
        invoke("island_drag_start").catch(() => {});
      }}
    >
      {dock.hidden && !edgeDelay && phase !== "panel" ? (
        // 贴边隐藏态：顶部横线/左右端帽信标；面板收起后才渲染，避免错位闪现。
        // edgeDelay＝滑出动画期间继续渲染胶囊（#3 延迟切换，见 dock effect 注释）。
        // peekW＝收纳宽（mini_line_width），顶部横线以此宽度居中呈现
        <EdgeTab
          edge={dock.edge}
          snap={visibleSnap}
          thresholds={thresholds}
          peekW={metrics.mini_line_w}
        />
      ) : (
        <>
          <IslandBar
            snap={visibleSnap}
            thresholds={thresholds}
            enterEdge={peekEnter}
            onToggle={togglePanel}
            mode={phase === "mini" ? "mini" : "full"}
            miniW={metrics.mini_line_w}
          />
          {/* 面板挂载由相位驱动：in=入场/展开，out=退场动画在播（播完 onAnimationEnd 卸载） */}
          {panelPhase && visibleSnap && (
            <div
              className={`panel-wrap ${panelPhase === "in" ? "panel-enter" : "panel-exit"}`}
              style={{ maxHeight: expandedCap - metrics.collapsed_h - PANEL_GAP_PX }}
              onAnimationEnd={(e) => {
                if (e.target === e.currentTarget && panelPhase === "out") {
                  setPanelPhase(null); // 退场播完才卸载，窗口高度 effect 随之收缩
                }
              }}
            >
              <Panel snap={visibleSnap} thresholds={thresholds} onNaturalHeight={setPanelH} />
            </div>
          )}
        </>
      )}
    </div>
  );
}

/**
 * 按设置过滤要展示的 Agent。后端 Aggregator 已按 agents_enabled 跳过未勾选
 * Agent 的扫描与采集，此处过滤是展示层兜底（设置推送与快照到达存在竞态窗口）
 */
function filterSnap(
  snap: IslandSnapshot | null,
  agents: string[],
): IslandSnapshot | null {
  if (!snap) return null;
  return { ...snap, sessions: snap.sessions.filter((s) => agents.includes(s.agent)) };
}

/** 路由真值源（2026-10-16 懒创建修复）：非岛窗口一律按窗口 label 路由——
 *  动态创建窗口的 URL fragment 存在不可靠路径（白屏事故的一半根因：hash
 *  丢失时误渲染灵动岛组件＝大片空白），label 由 Tauri 保证可靠；
 *  hash 仅作岛窗口（静态配置、无 label 分流需求）的兜底 */
function windowRoute(): string {
  const label = getCurrentWebviewWindow().label;
  return label !== "island" ? `#${label}` : window.location.hash;
}

/** 按窗口分流：设置窗口 / 报表窗口 / 额度窗口 / 会话窗口 / 关于窗口 / 托盘菜单窗口 / 灵动岛窗口 */
function App() {
  const route = windowRoute();
  if (route === "#settings") {
    // lazy 分割（四轮审查补漏）：设置页原是唯一静态导入的窗口页，打进岛窗口
    // 入口 chunk（实测 306KB）；与其余五窗「独立窗口独立加载」的既有注释意图对齐。
    // 每页独立错误边界（2026-10-16 审查）：原先仅 main.tsx 一层根边界，任一页
    // 子组件渲染崩溃整窗降级；lazy chunk 加载失败（Suspense fallback=null）更是
    // 静默白屏——分层后崩溃只降级本页且带组件栈留痕
    return (
      <ErrorBoundary name="settings">
        <Suspense fallback={null}>
          <Settings />
        </Suspense>
      </ErrorBoundary>
    );
  }
  if (route === "#tray-menu") {
    return (
      <ErrorBoundary name="tray-menu">
        <Suspense fallback={null}>
          <TrayMenu />
        </Suspense>
      </ErrorBoundary>
    );
  }
  if (route === "#tooltip") {
    return (
      <ErrorBoundary name="tooltip">
        <Suspense fallback={null}>
          <TooltipWindow />
        </Suspense>
      </ErrorBoundary>
    );
  }
  if (route === "#report") {
    return (
      <ErrorBoundary name="report">
        <Suspense fallback={null}>
          <Report />
        </Suspense>
      </ErrorBoundary>
    );
  }
  if (route === "#quotas") {
    return (
      <ErrorBoundary name="quotas">
        <Suspense fallback={null}>
          <Quotas />
        </Suspense>
      </ErrorBoundary>
    );
  }
  if (route === "#sessions") {
    return (
      <ErrorBoundary name="sessions">
        <Suspense fallback={null}>
          <Sessions />
        </Suspense>
      </ErrorBoundary>
    );
  }
  if (route === "#about") {
    return (
      <ErrorBoundary name="about">
        <Suspense fallback={null}>
          <About />
        </Suspense>
      </ErrorBoundary>
    );
  }
  return <IslandApp />;
}

export default App;
