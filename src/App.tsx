/**
 * 应用入口：按窗口 URL hash 分流 —— #settings 渲染设置页（常规窗口），
 * #report 渲染报表页（常规窗口），#about 渲染关于页（常规窗口），
 * 其余渲染灵动岛（透明窗口）；岛消费 island-snapshot 快照，hover 展开面板
 */
import { lazy, Suspense, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { LogicalSize } from "@tauri-apps/api/dpi";
import IslandBar from "./island/IslandBar";
import Panel from "./island/Panel";
import EdgeTab from "./island/EdgeTab";
import Settings from "./Settings";
import { AGENT_DEFS, setAgentColors } from "./shared/types";
import { useTheme } from "./shared/theme";
import {
  ISLAND_OPACITY_EVENT,
  applyIslandOpacity,
  asIslandOpacity,
} from "./shared/islandOpacity";
import type { IslandSnapshot, Thresholds } from "./shared/types";
import "./App.css";

// 报表页 lazy 分割：echarts 只在报表窗口加载，岛窗口 bundle 不受影响（01-RESEARCH §9）
const Report = lazy(() => import("./report/Report"));
// 会话窗口 lazy 分割（M1-10）：与报表页同款——独立窗口独立加载，
// 不含 echarts；岛窗口 bundle 不受影响
const Sessions = lazy(() => import("./sessions/Sessions"));
// 关于页 lazy 分割：与报表页同款——独立窗口独立加载，岛窗口 bundle 不受影响
const About = lazy(() => import("./about/About"));
// 托盘菜单页 lazy 分割：托盘右键弹出的自绘菜单（M1-9），独立窗口独立加载
const TrayMenu = lazy(() => import("./traymenu/TrayMenu"));

/** 岛自适应尺寸（Rust island_metrics：显示器逻辑宽 × 30%，夹取 380–800） */
interface IslandMetrics {
  width: number;
  collapsed_h: number;
  expanded_h: number;
  /** 顶部贴边隐藏态宽度（胶囊公式常数减半，恒 2:1） */
  peek_top_w: number;
}
const DEFAULT_METRICS: IslandMetrics = {
  width: 360,
  collapsed_h: 48,
  expanded_h: 520,
  peek_top_w: 288,
};

/** 面板与胶囊的间距（与 App.css .panel-wrap 的 margin-top 保持一致） */
const PANEL_GAP_PX = 6;
/** 展开态窗口高度下限：空会话时骨架（汇总条+标题+空态+额度区）仍完整可用的最小高度 */
const MIN_EXPANDED_H = 240;

/** 阈值默认值与脏数据防御（R5：设置页可配，启动时加载一次，重启生效） */
const DEFAULT_THRESHOLDS: Thresholds = { warn: 80, danger: 95 };

function sanitizeThresholds(warn: unknown, danger: unknown): Thresholds {
  const w = Number(warn);
  const d = Number(danger);
  const ok = (v: number) => Number.isFinite(v) && v > 0 && v <= 100;
  if (!ok(w) || !ok(d) || w >= d) return DEFAULT_THRESHOLDS;
  return { warn: w, danger: d };
}

function IslandApp() {
  // 主题应用与跟随（M1-4）：结果写 <html> 的 data-theme，CSS 变量自动切换；岛无需感知返回值
  useTheme();
  const [snap, setSnap] = useState<IslandSnapshot | null>(null);
  const [expanded, setExpanded] = useState(false);
  // 面板动画相位：in=挂载且入场中/展开，out=退场动画在播，null=未挂载。
  // 退场必须等动画播完才卸载并缩窗——窗口先缩会拦腰截断动画
  const [panelPhase, setPanelPhase] = useState<"in" | "out" | null>(null);
  // 面板内容自然高度（Panel 上报）：展开高度自适应的依据，null = 未测得
  const [panelH, setPanelH] = useState<number | null>(null);
  // 胶囊"由远及近"入场：隐藏态滑回显示的瞬间置为贴边边（决定动画 origin 与位移方向）。
  // 仅悬停路径消费（标签→胶囊翻转可见）；托盘召唤走物理滑入，无翻转不触发——
  // 召唤时若重播动画会出现"胶囊先出现又闪回动画起点"的跳变（2026-09-20 实测）
  const [peekEnter, setPeekEnter] = useState<string | null>(null);
  const prevHiddenRef = useRef(false);
  // 上次实际下发的窗口高度（防循环护栏：观察器→setSize→resize→观察器）
  const lastSetH = useRef<number>(0);
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
  // 贴边隐藏态下"滑入完成后再展开"的定时器
  const enterTimer = useRef<number | undefined>(undefined);
  // 移出后滑出隐藏的宽限定时器（400ms 内回来则取消，防误触）
  const leaveTimer = useRef<number | undefined>(undefined);
  // 托盘"显示"召唤后的自动收起定时器（3s；鼠标移入即取消——用户正在看/用岛）
  const summonTimer = useRef<number | undefined>(undefined);

  // 启动引导：读取岛尺寸/提醒阈值/悬停展开开关/贴边状态。
  // 初始贴边状态必须主动拉取：启动恢复在 setup 阶段已把窗口滑出隐藏，
  // 早于本窗口事件监听建立，island-dock 事件收不到；不拉取会把隐藏态渲染成完整胶囊。
  // 窗口极早期加载（暖缓存下亚秒级完成）时，挂载瞬间的 invoke 会被无声丢弃且
  // promise 永不落定（2026-09-17 实测），故每 1s 重试直至三项各成功一次；
  // 成功后停表，之后贴边状态改由 island-dock 事件流维护
  useEffect(() => {
    const done = { metrics: false, dock: false, settings: false };
    const bootstrap = async () => {
      if (!done.metrics) {
        try {
          const m = await invoke<IslandMetrics>("island_metrics");
          if (m) setMetrics(m); // 取不到显示器时后端返回 null，保持默认尺寸即可
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
          done.dock = true;
        } catch {
          /* 通道未就绪，下轮重试 */
        }
      }
      if (!done.settings) {
        try {
          const s = await invoke<Record<string, string>>("get_settings");
          setThresholds(sanitizeThresholds(s.threshold_warn, s.threshold_danger));
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
          // 背景不透明度：搭 bootstrap 重试便车——极早期 invoke 永不落定时靠 1s 重试兜住
          applyIslandOpacity(asIslandOpacity(s.island_opacity));
          done.settings = true;
        } catch {
          /* 通道未就绪，下轮重试 */
        }
      }
    };
    void bootstrap();
    const timer = window.setInterval(() => {
      if (done.metrics && done.dock && done.settings) {
        window.clearInterval(timer);
        return;
      }
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

  // 设置页修改监控 Agent 列表/颜色后实时推送
  useEffect(() => {
    const un = listen<{ agents: string[]; colors: Record<string, string> }>(
      "agents-changed",
      (e) => {
        setAgents(e.payload.agents);
        setAgentColors(e.payload.colors);
      },
    );
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

  // 订阅贴边状态（吸附/滑出/滑入时由 Rust 推送）
  useEffect(() => {
    const un = listen<{ edge: string; hidden: boolean }>("island-dock", (e) => {
      dockRef.current = e.payload;
      setDock(e.payload);
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  // 隐藏 → 显示的瞬间标记胶囊入场动画（悬停滑入与托盘召唤共用此链路）；
  // prevHiddenRef 只在 dock 变化时推进，避免启动引导首次拉取误触发
  useEffect(() => {
    if (prevHiddenRef.current && !dock.hidden && dock.edge !== "none") {
      setPeekEnter(dock.edge);
    }
    prevHiddenRef.current = dock.hidden;
  }, [dock]);

  // expanded 翻转映射到面板相位：展开即挂载入场；收起先进退场动画，播完自动卸载
  useEffect(() => {
    if (expanded) setPanelPhase("in");
    else setPanelPhase((p) => (p === "in" ? "out" : p));
  }, [expanded]);

  // 托盘"显示"召唤（island-summon）：贴边停靠的岛滑回后 3s 自动滑出收回——
  // 召唤是"临时亮位提醒"，无人理会就自己收好；鼠标移入则取消（交给常规
  // 移出滑出逻辑接管），期间被托盘隐藏或拖走也不动作
  useEffect(() => {
    const un = listen("island-summon", () => {
      // 召唤的入场由 Rust 物理滑入承担（island_transition Pill），此处只负责
      // 3s 自动收回计时；不重播 CSS 入场动画（避免胶囊已亮出又闪回动画起点）
      window.clearTimeout(summonTimer.current);
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

  useEffect(() => {
    const unlisten = listen<IslandSnapshot>("island-snapshot", (e) =>
      setSnap(e.payload),
    );
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // 展开/收起：仅调高度；失败不致命（尺寸权限缺失时内容被裁剪但不崩溃）。
  // 展开高度按面板内容自适应（2026-09-18 展示改造）：
  //   目标 = 胶囊 + 间距 + 面板自然高度，上限 expanded_h（超出面板内部滚动），
  //   下限 MIN_EXPANDED_H；未测得前回退 expanded_h（与历史行为一致，避免闪缩）。
  // 收起时序（2026-09-20 动效改造）：面板退场动画在播（phase=out）时窗口保持
  // 原高不动，动画播完卸载（phase=null）后才收缩——提前缩窗会拦腰截断动画。
  // 窗口必须跟随内容收缩：岛常驻顶层，若只缩内容不缩窗口，
  // 下方透明区域会拦截鼠标、挡住下层应用点击
  useEffect(() => {
    if (panelPhase == null && panelH != null) setPanelH(null); // 面板卸载后清测量值，下次展开重新上报
    if (!expanded && panelPhase != null) return; // 退场动画在播：窗口高度按兵不动
    const win = getCurrentWebviewWindow();
    const target = expanded
      ? Math.min(
          metrics.expanded_h,
          Math.max(
            MIN_EXPANDED_H,
            metrics.collapsed_h + PANEL_GAP_PX + (panelH ?? metrics.expanded_h),
          ),
        )
      : metrics.collapsed_h;
    if (target === lastSetH.current) return; // 目标不变不重下发，断开反馈环
    lastSetH.current = target;
    win
      .setSize(new LogicalSize(metrics.width, target))
      .catch(() => {});
  }, [expanded, metrics, panelH, panelPhase]);

  // 进入贴边隐藏态时自动收起面板（Rust 已把窗口缩回收缩态，保持渲染一致）
  useEffect(() => {
    if (dock.hidden && expanded) setExpanded(false);
  }, [dock.hidden, expanded]);

  const visibleSnap = filterSnap(snap, agents);

  return (
    <div
      className="root"
      onMouseEnter={() => {
        window.clearTimeout(leaveTimer.current);
        // 用户正在看/用岛：托盘召唤的自动收起作废，收起交给移出逻辑
        window.clearTimeout(summonTimer.current);
        if (dockRef.current.hidden) {
          // 贴边隐藏态：先滑入显示独立标签→胶囊；悬停展开开启时滑入完成后再展开面板
          invoke("island_peek", { show: true }).catch(() => {});
          window.clearTimeout(enterTimer.current);
          if (hoverCard) {
            enterTimer.current = window.setTimeout(() => setExpanded(true), 260);
          }
        } else if (hoverCard) {
          setExpanded(true);
        }
      }}
      onMouseDown={() => {
        // 拖拽开始：取消在播滑动动画，防止程序化移动打断系统拖拽
        invoke("island_drag_start").catch(() => {});
      }}
      onMouseLeave={() => {
        window.clearTimeout(enterTimer.current);
        setExpanded(false);
        if (dockRef.current.edge !== "none") {
          // 400ms 宽限后滑出隐藏（期间重新进入则取消）
          window.clearTimeout(leaveTimer.current);
          leaveTimer.current = window.setTimeout(() => {
            invoke("island_peek", { show: false }).catch(() => {});
          }, 400);
        }
      }}
    >
      {dock.hidden && !expanded ? (
        // 贴边隐藏态：独立信息标签（非胶囊截取）；面板收起完成后才渲染，避免错位闪现。
        // peekW：标签宽度（胶囊常数减半），窗口保持全宽，由 CSS 居中呈现窄标签
        <EdgeTab
          edge={dock.edge}
          snap={visibleSnap}
          thresholds={thresholds}
          peekW={metrics.peek_top_w}
        />
      ) : (
        <>
          <IslandBar
            snap={visibleSnap}
            thresholds={thresholds}
            enterEdge={peekEnter}
            onToggle={hoverCard ? undefined : () => setExpanded((v) => !v)}
          />
          {/* 面板挂载由相位驱动：in=入场/展开，out=退场动画在播（播完 onAnimationEnd 卸载） */}
          {panelPhase && visibleSnap && (
            <div
              className={`panel-wrap ${panelPhase === "in" ? "panel-enter" : "panel-exit"}`}
              style={{ maxHeight: metrics.expanded_h - metrics.collapsed_h - PANEL_GAP_PX }}
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

/** 按 URL hash 分流：设置窗口 / 报表窗口 / 会话窗口 / 关于窗口 / 托盘菜单窗口 / 灵动岛窗口 */
function App() {
  if (window.location.hash === "#settings") {
    return <Settings />;
  }
  if (window.location.hash === "#tray-menu") {
    return (
      <Suspense fallback={null}>
        <TrayMenu />
      </Suspense>
    );
  }
  if (window.location.hash === "#report") {
    return (
      <Suspense fallback={null}>
        <Report />
      </Suspense>
    );
  }
  if (window.location.hash === "#sessions") {
    return (
      <Suspense fallback={null}>
        <Sessions />
      </Suspense>
    );
  }
  if (window.location.hash === "#about") {
    return (
      <Suspense fallback={null}>
        <About />
      </Suspense>
    );
  }
  return <IslandApp />;
}

export default App;
