/**
 * 自绘悬浮提示（2026-09-18 展示改造 G2，同日两次修正后定稿）：
 * portal 挂 body——不受面板滚动容器 overflow 裁剪；400ms 延迟出现，
 * 快速滑过不闪烁。
 *
 * 实现约定（全项目悬浮气泡统一规则，详见 docs/02-DESIGN.md §5）：
 * - cloneElement 向触发元素注入 onMouseEnter/onMouseLeave，不包裹额外 DOM——
 *   触发元素保持在原布局位置（flex/width 等样式零改动）；
 *   注意：触发元素自身的鼠标事件会被接管（当前无此类用法）；
 * - 水平跟随光标（整行宽的触发源如历史折叠行，气泡出现在鼠标处而非行中心）；
 * - 垂直锚定触发元素下缘（放不下翻转到上缘）；
 * - 渲染后用 useLayoutEffect 量气泡真实宽高再夹取——不预设尺寸常量，
 *   绘制发生在布局效应完成之后，两段定位无闪烁；
 * - 仅悬停时挂载 portal 节点，空闲零 DOM——列表大规模实例化（每行挂 Tip）安全
 *
 * ⚠ 使用边界：仅限常规/展开窗口（面板、设置、报表）。小微窗口（胶囊收缩态
 * 48px、贴边隐藏态）会被透明窗口边界裁剪，一律用原生 title（OS 级渲染可越窗）
 */
import {
  cloneElement,
  useCallback,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { createPortal } from "react-dom";

/** 延迟出现毫秒数（原生 title 约 1s，这里稍快但仍防误触） */
const SHOW_DELAY_MS = 400;
/** 气泡与视口边缘的最小间距（像素） */
const EDGE_GAP = 8;
/** 气泡与触发元素的垂直间距（像素） */
const ANCHOR_GAP = 6;

/** 第一阶段：待测量（只有触发几何与光标位置，气泡真实尺寸未知） */
interface Measure {
  cursorX: number;
  anchorTop: number;
  anchorBottom: number;
}

/** 第二阶段：真实尺寸量出后的最终定位 */
interface TipPos {
  x: number;
  y: number;
}

export default function Tip({
  content,
  placement = "bottom",
  children,
}: {
  /** 气泡内容（支持多行节点） */
  content: React.ReactNode;
  /** 弹出方位：bottom=触发元素下方（默认，历史行为）；top=上方——
   *  表头等贴顶触发源用 top，避免气泡正好盖住紧邻其下的第一行数据 */
  placement?: "top" | "bottom";
  /** 触发元素（单个可接收鼠标事件的元素） */
  children: React.ReactElement<React.HTMLAttributes<HTMLElement>>;
}) {
  const [measure, setMeasure] = useState<Measure | null>(null);
  const [pos, setPos] = useState<TipPos | null>(null);
  const tipRef = useRef<HTMLDivElement>(null);
  const timer = useRef<number | undefined>(undefined);

  /** 进入：记录光标横坐标；延迟到显示时刻才测触发元素矩形（面板高度自适应
   *  改变窗口时也能拿到最新位置，避免坐标过期） */
  const onEnter = useCallback((e: React.MouseEvent) => {
    const el = e.currentTarget as HTMLElement;
    const cursorX = e.clientX; // 合成事件属性仅在派发期间有效，同步取值
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      const r = el.getBoundingClientRect();
      setMeasure({ cursorX, anchorTop: r.top, anchorBottom: r.bottom });
    }, SHOW_DELAY_MS);
  }, []);

  /** 离开：清定时器并收起 */
  const onLeave = useCallback(() => {
    window.clearTimeout(timer.current);
    setMeasure(null);
    setPos(null);
  }, []);

  // 第二阶段：气泡已渲染（先隐藏），量真实宽高 → 跟光标夹取 + 上下翻转 →
  // 设最终坐标后同帧绘制（layout effect 在绘制前同步执行，无闪烁）
  useLayoutEffect(() => {
    if (!measure) return;
    const tip = tipRef.current;
    if (!tip) return;
    const b = tip.getBoundingClientRect();
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    // 水平：气泡中心对齐光标，整体夹进视口
    const x = Math.min(
      Math.max(measure.cursorX - b.width / 2, EDGE_GAP),
      vw - b.width - EDGE_GAP,
    );
    // 垂直：按 placement 优先方位，放不下翻到另一侧，仍放不下贴视口边
    const belowY = measure.anchorBottom + ANCHOR_GAP;
    const aboveY = measure.anchorTop - ANCHOR_GAP - b.height;
    const fitsBelow = belowY + b.height <= vh - EDGE_GAP;
    const fitsAbove = aboveY >= EDGE_GAP;
    const y =
      placement === "top"
        ? fitsAbove
          ? aboveY
          : Math.min(belowY, vh - b.height - EDGE_GAP)
        : fitsBelow
          ? belowY
          : Math.max(aboveY, vh - b.height - EDGE_GAP);
    setPos({ x, y });
  }, [measure, placement]);

  return (
    <>
      {cloneElement(children, { onMouseEnter: onEnter, onMouseLeave: onLeave })}
      {measure != null &&
        createPortal(
          <div
            ref={tipRef}
            className="tip"
            role="tooltip"
            style={{
              left: pos?.x ?? -9999,
              top: pos?.y ?? -9999,
              visibility: pos ? "visible" : "hidden",
            }}
          >
            {content}
          </div>,
          document.body,
        )}
    </>
  );
}
