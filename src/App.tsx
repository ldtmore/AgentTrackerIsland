/**
 * 应用入口：按窗口 URL hash 分流 —— #settings 渲染设置页（常规窗口），
 * #report 渲染报表页（常规窗口），#quotas 渲染额度页（常规窗口，M3-5），
 * #about 渲染关于页（常规窗口），
 * 其余渲染灵动岛（透明窗口）；岛消费 island-snapshot 快照，hover 展开面板
 */
import { lazy, Suspense, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
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

/** 岛自适应尺寸（Rust island_metrics：显示器逻辑宽 × 30%，夹取 380–800） */
interface IslandMetrics {
  width: number;
  collapsed_h: number;
  expanded_h: number;
  /** 顶部贴边隐藏态宽度（胶囊公式常数减半，恒 2:1） */
  peek_top_w: number;
  /** 工作区底缘（逻辑 y）：展开封顶用（五轮审查批次三）——岛窗口左上角锚定、
   *  面板向下生长，岛在屏幕下半部时须按「底缘 − 岛 y」限制展开高度 */
  work_bottom: number;
}
const DEFAULT_METRICS: IslandMetrics = {
  // 380 与 tauri.conf 的岛窗出生宽一致（五轮审查：原 360 会误入 w-sm 档一帧）
  width: 380,
  collapsed_h: 48,
  expanded_h: 520,
  peek_top_w: 288,
  work_bottom: 672,
};

/** 面板与胶囊的间距（与 App.css .panel-wrap 的 margin-top 保持一致） */
const PANEL_GAP_PX = 6;
/** 展开态窗口高度下限：空会话时骨架（汇总条+标题+空态+额度区）仍完整可用的最小高度 */
const MIN_EXPANDED_H = 240;

// 阈值默认值与清洗已收敛至 shared/types（四轮审查：原先 App/Quotas/TrayMenu
// 三处各写一份 80/95/10/5 与逐项校验，口径有漂移风险）——R5：设置页可配，
// 启动时加载一次，重启生效

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
  // 贴边隐藏态下"滑入完成后再展开"的定时器
  const enterTimer = useRef<number | undefined>(undefined);
  // 移出后滑出隐藏的宽限定时器（400ms 内回来则取消，防误触）
  const leaveTimer = useRef<number | undefined>(undefined);
  // 面板收起宽限定时器（五轮审查 P1 修复）：见根节点 onMouseLeave 注释
  const collapseTimer = useRef<number | undefined>(undefined);
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
    // 展示常量同步（#20）：「已结束」阈值从后端拉取（Rust 单真值源），失败保持默认
    void syncDisplayConstants();
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
      if (done.metrics && done.dock && done.settings) {
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
    if (!expanded) return;
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
  }, [expanded, metrics]);

  // 展开态窗口高度上限：屏幕常量与「下方可用高度」取小；与 MIN_EXPANDED_H
  // 冲突时保下限（宁可贴底微溢出，不出畸形窄条）
  const expandedCap = Math.max(
    MIN_EXPANDED_H,
    Math.min(metrics.expanded_h, availBelow ?? metrics.expanded_h),
  );

  useEffect(() => {
    if (panelPhase == null && panelH != null) setPanelH(null); // 面板卸载后清测量值，下次展开重新上报
    if (!expanded && panelPhase != null) return; // 退场动画在播：窗口高度按兵不动
    const win = getCurrentWebviewWindow();
    const target = {
      w: metrics.width,
      h: expanded
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
  }, [expanded, metrics, panelH, panelPhase, expandedCap]);

  // 进入贴边隐藏态时自动收起面板（Rust 已把窗口缩回收缩态，保持渲染一致）
  useEffect(() => {
    if (dock.hidden && expanded) setExpanded(false);
  }, [dock.hidden, expanded]);

  // 显示器/缩放变化后重拉岛尺寸（2026-10-03 审查修复）：metrics 原先仅挂载
  // 拉取一次，拖岛跨屏或系统缩放变化后宽度档位/标签宽仍用旧值；贴边吸附
  // （island-dock）是拖动结束的天然检查点，顺带重拉
  useEffect(() => {
    const refresh = () => {
      invoke<IslandMetrics>("island_metrics")
        .then((m) => {
          if (m) setMetrics(m);
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
  // metrics 未就绪的默认 360 归 w-sm（兜底最小形态，就绪后由事件刷新）
  const widthClass =
    metrics.width >= 560 ? "w-lg" : metrics.width >= 460 ? "w-md" : "w-sm";

  return (
    <div
      className={`root ${widthClass}`}
      onMouseEnter={() => {
        window.clearTimeout(leaveTimer.current);
        window.clearTimeout(collapseTimer.current);
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
        // 收起加 150ms 宽限（五轮审查 P1 修复）：面板内经 portal 挂 body 的
        // 弹层（历史筛选菜单）不在 .root 子树内——React mouseleave 按命中元素
        // 的 DOM 祖先链判定，指针移入菜单瞬间根节点就会收到 mouseleave，
        // 无宽限则面板连带菜单当场塌掉（160ms 退场后菜单一起卸载），筛选
        // 实际不可用。到期再探测：弹层/面板/根任一仍悬停则不收（菜单自身
        // 的 200ms 关闭宽限只护它的计时器，救不了根节点）
        window.clearTimeout(collapseTimer.current);
        collapseTimer.current = window.setTimeout(() => {
          if (
            document.querySelector(".history-menu:hover") ||
            document.querySelector(".panel-wrap:hover") ||
            document.querySelector(".root:hover")
          )
            return;
          setExpanded(false);
        }, 150);
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

/** 按 URL hash 分流：设置窗口 / 报表窗口 / 额度窗口 / 会话窗口 / 关于窗口 / 托盘菜单窗口 / 灵动岛窗口 */
function App() {
  if (window.location.hash === "#settings") {
    // lazy 分割（四轮审查补漏）：设置页原是唯一静态导入的窗口页，打进岛窗口
    // 入口 chunk（实测 306KB）；与其余五窗「独立窗口独立加载」的既有注释意图对齐
    return (
      <Suspense fallback={null}>
        <Settings />
      </Suspense>
    );
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
  if (window.location.hash === "#quotas") {
    return (
      <Suspense fallback={null}>
        <Quotas />
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
