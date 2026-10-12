/**
 * 贴边隐藏态（M1-6 立项；2026-10-12 07-TOP-ISLAND 改版拆分）：
 * - **顶部＝信标横线**（TopLine）：3px 状态色线贴屏幕顶边（宽 mini_line_w 居中于
 *   停靠位），替代旧 14px 底部切片——旧形态是"胶囊被遮住的残缺局部"（下凸额头），
 *   新形态是"完整的指示物"（借鉴 iOS 底部 Home Indicator 移到顶部并赋予状态色）。
 *   唯一内容通道＝状态色＋节律（通道补偿），刻意不做分段/额度/"！"（D2/D3）；
 *   可见 3px＋命中 16px 热区（可见区≠命中区），预响应（hover 提亮增厚）在 CSS。
 * - **左右＝端帽弓形＋药丸点列**（2026-10-08「弓形＋药丸」改版，不动）：
 *   颜色＝Agent 身份（竖向药丸点列，密度恒定＋中轴放置），状态用亮度/动效表达；
 *   额度线＝端帽外沿弧（dasharray=已用%，tensestAccount 同源选择器）；
 *   出错＝药丸外描边红圈＋轮廓光红；采集异常＝轮廓光琥珀。
 *
 * ⚠ 本区域明确不做 tooltip（2026-09-18 与用户确认的伪需求，勿再实现）：
 * 根容器的 onMouseEnter 在鼠标进入隐藏态瞬间触发 island_peek 滑入，
 * 本组件随即卸载换成大岛渲染——任何悬停提示都没有稳定展示时机
 */
import { useEffect, useRef, useState } from "react";
import type { IslandSnapshot, SessionState, Thresholds } from "../shared/types";
import { agentColor, quotaLevel, tensestAccount } from "../shared/types";
import { agentLimit, orderAgentIds } from "../shared/agentOrder";

/** 按 Agent 聚合最严重状态，等宽分段（大小不编码信息）；
 *  全离线 Agent 不占位（E2） */
function agentSegments(snap: IslandSnapshot | null) {
  if (!snap) return [];
  const severity: Record<string, number> = {
    error: 4,
    waiting: 3,
    working: 2,
    idle: 1,
    offline: -1,
  };
  const worst = new Map<string, SessionState>();
  for (const s of snap.sessions) {
    const cur = worst.get(s.agent);
    if (cur === undefined || (severity[s.state] ?? 0) > (severity[cur] ?? 0)) {
      worst.set(s.agent, s.state);
    }
  }
  // 过滤全离线：该 Agent 进程已全部退出，分段不再渲染；
  // 排序＝用户拖拽序（2026-10-03，注册表由 App 启动/事件注入；替换旧
  // localeCompare 字母序——与设置页卡片/面板筛选菜单同序）；
  // 前 N 上岛（2026-10-08）：拖拽序前 N 个（默认 5、上限 6＝药丸列满配
  // 容量），顶部分段与左右药丸同源同限（三处同一展示集）
  const ranked = orderAgentIds([...worst.keys()]);
  return ranked
    .slice(0, agentLimit())
    .map((agent) => ({ agent, state: worst.get(agent)! }))
    .filter((s) => s.state !== "offline");
}

/** 隐藏态额度信号（批次三 F1/F5）：与胶囊同源的选择器 + 三态结果。
 *  pct=null 且 hasData=true ⇒ 展示集有实例但暂无可用数据（画空轨道）；
 *  hasData=false ⇒ 展示集为空（整线隐藏，尊重显式选择） */
function quotaSignal(snap: IslandSnapshot | null, thresholds: Thresholds): {
  pct: number | null;
  level: "normal" | "warn" | "danger";
  hasData: boolean;
} {
  const islandAccounts = (snap?.accounts ?? []).filter((a) => a.in_island);
  const pick = islandAccounts.length > 0 ? tensestAccount(islandAccounts, thresholds) : null;
  if (pick?.reason === "balance") {
    // 余额跌破警戒线：满线红即「额度告急」信号（微 UI 无金额位，降级表达）
    return { pct: 100, level: "danger", hasData: true };
  }
  if (pick?.reason === "window" && pick.quota) {
    const pct = Math.min(100, Math.max(0, pick.quota.used_percent ?? 0));
    return { pct, level: quotaLevel(pct, thresholds.warn, thresholds.danger), hasData: true };
  }
  return { pct: null, level: "normal", hasData: islandAccounts.length > 0 };
}

/** 额度档位 → 弧线颜色（与轮廓线/进度条档位色一致） */
const ARC_STROKE: Record<string, string> = {
  normal: "#34d399",
  warn: "#fbbf24",
  danger: "#f87171",
};

/** 横线状态类推导（07 §6.2）：出错（含额度耗尽）＞等待＞采集异常（仅无前两者时，
 *  与等待同为琥珀、靠节律区分性质）＞工作中；空闲/无会话＝默认中灰（无类） */
function topLineState(snap: IslandSnapshot | null): string {
  if (snap?.island === "any_error" || snap?.quota_exhausted === true) return "st-error";
  if (snap?.island === "any_waiting") return "st-waiting";
  if (snap?.degraded === true) return "st-degraded";
  if (snap?.island === "any_working") return "st-working";
  return "";
}

/** 顶部信标横线（07 §3.1）：3px 状态色线，唯一内容通道＝色＋节律。
 *  一次性状态标点（2026-10-13 三轮审查 #4）：进入出错/等待的瞬间播一次
 *  宽度脉冲（~0.6s 自熄）——岛端有 halo 光环做「状态变化的标点」，横线此前
 *  没有对等物，状态迁移错过就错过。选宽度而非增厚：增厚已被 hover 预响应
 *  占用（词汇单一性）；仅升级态触发（进入出错/等待），首帧即处该态（启动
 *  带病上线）不播——与岛端 halo 的 was==null 守卫同款；reduced-motion
 *  由全局收敛兜底 */
function TopLine({ snap, peekW }: { snap: IslandSnapshot | null; peekW: number }) {
  const stCls = topLineState(snap);
  const [beat, setBeat] = useState(false);
  const prevStRef = useRef<string | null>(null);
  useEffect(() => {
    const was = prevStRef.current;
    prevStRef.current = stCls;
    if (was == null || was === stCls) return;
    if (stCls !== "st-error" && stCls !== "st-waiting") return;
    setBeat(true);
    const t = window.setTimeout(() => setBeat(false), 700);
    return () => window.clearTimeout(t);
  }, [stCls]);
  // 宽度防御（07 §7）：后端旧版本缺 mini_line_w 字段时回落 200（undefined 会让
  // style.width 失效、回落 CSS 的 width:100%＝横线全宽，2026-10-12 实机事故；
  // 回落值与 Rust MINI_W_MIN 同源，B9 起 240→200——原注释"240"已过期一并修正）
  const lineW = Number.isFinite(peekW) && peekW > 0 ? peekW : 200;
  return (
    <div
      className={`top-line ${stCls}${beat ? " top-line-beat" : ""}`}
      style={{ width: lineW }}
      data-tauri-drag-region
    />
  );
}

export default function EdgeTab({
  edge,
  snap,
  thresholds,
  peekW,
}: {
  edge: string;
  snap: IslandSnapshot | null;
  thresholds: Thresholds;
  /** 顶部横线宽度（Rust mini_line_width：大岛宽×40% 夹取 [240， 320]，与小岛
   *  等宽——横线是"藏起来的小岛"的水平投影）。窗口保持全宽，横线以 CSS 居中
   *  呈现；两侧与下方透明热区由后端动态鼠标穿透放行点击（命中区 16px） */
  peekW: number;
}) {
  // 左右件的出错/降级描边语义（edge-error/edge-degraded 容器类）不变
  const error = snap?.island === "any_error" || snap?.quota_exhausted === true;
  const degraded = snap?.degraded === true && !error;
  const cls = `edge-tab edge-tab-${edge}${error ? " edge-error" : ""}${degraded ? " edge-degraded" : ""}`;
  const segments = agentSegments(snap);
  const q = quotaSignal(snap, thresholds);
  const pctClamped = q.pct != null ? Math.min(100, Math.max(0, q.pct)) : 0;

  // 顶部贴边：信标横线（07 §3.1）——3px 状态色线，宽度居中，唯一通道＝色＋节律
  // （一次性状态标点与宽度防御见 TopLine 子组件）
  if (edge !== "left" && edge !== "right") {
    return <TopLine snap={snap} peekW={peekW} />;
  }

  // 左右贴边（2026-10-08「弓形＋药丸」改版）：SVG 承载形状——真实端帽弓形
  // 背景＋轮廓光＋外沿额度弧；HTML 承载竖向药丸点列（8×4，中轴放置，密度
  // 恒定随内容生长，最多 6 粒＝上岛深度上限，恒不溢出）。
  // 出错表达＝药丸外描边红圈（保身份色完整）＋轮廓光转红，不塞「！」
  const mirror = edge === "right";
  // 端帽弓形几何：过弦两端 (0,0)/(0,48) 与弧顶 (20,24) 的圆，
  // r = (24² + 20²) / (2×20) = 24.4（胶囊端帽圆头 r=24 的忠实拟合，误差 <0.4px）。
  // 左贴边＝屏幕边在左（x=0 弦）、弓形鼓向屏幕内；右贴边镜像。
  // ⚠ 弧线 sweep flag 经实机校准：左贴逆时针（0）、右贴顺时针（1），
  // 反了会画成贴弦对侧的透镜形（2026-10-08 实测留痕，勿再翻转）
  const CAP_R = 24.4;
  const bodyPath = mirror
    ? `M 20 0 L 20 48 A ${CAP_R} ${CAP_R} 0 0 1 20 0 Z`
    : `M 0 0 L 0 48 A ${CAP_R} ${CAP_R} 0 0 0 0 0 Z`;
  // 外沿弧（额度线与轮廓光共用，贴容器边缘）；dasharray 从上端沿弧向下端推进。
  // ⚠ 额度线经 clipPath 裁进弓形（见 JSX）——「中心线贴边、外半露在壁纸上」
  // 的旧画法在浅色壁纸上呈「色环漂浮在标签外」（2026-10-08 所有者两轮截图
  // 实测的返工点），顶部件的轮廓线画在切片内、左右件必须同一语义
  const capArc = mirror
    ? `M 20 0 A ${CAP_R} ${CAP_R} 0 0 0 20 48`
    : `M 0 0 A ${CAP_R} ${CAP_R} 0 0 1 0 48`;
  return (
    <div className={cls} data-tauri-drag-region>
      <svg className="edge-svg" viewBox="0 0 20 48" preserveAspectRatio="none">
        <defs>
          {/* 额度线裁剪域＝弓形本体：线的可见部分完全落在端帽内侧
              （与顶部件「轮廓线画在切片内」同一语义，壁纸上不再显色） */}
          <clipPath id="edge-cap-clip"><path d={bodyPath} /></clipPath>
        </defs>
        {/* 弓形本体：真实端帽切片的玻璃填充（与胶囊同一套配方，B30——随主题
            与不透明度滑块；形状真源在 SVG——矩形 div 画不出
            单边整条圆弧，CSS border-radius 会被相邻半径规则压缩成尖椭圆） */}
        <path d={bodyPath} className="edge-body" />
        {/* 额度线（底轨＋档位弧）：包两层 g——外层裁进弓形，内层 filter 让
            档位微光随线生成后再一并被外层裁进弓形（形内辉光）。2026-10-09
            直角修复：旧版 filter 挂在裁剪外层，光晕越过窗口边界（弧顶贴窗口
            右缘、弧端贴顶底缘，余量为零）被硬切成直角轮廓（黑壁纸实机取证），
            光晕一律收进形内。dasharray 经 style 下发：用量变化时弧长平滑
            补间；color 供内层 g currentColor 取档位色，空轨道态（F5，
            pct=null）回退 transparent——底轨保持中性不带色光 */}
        <g clipPath="url(#edge-cap-clip)">
          <g
            filter="drop-shadow(0 0 2.5px currentColor)"
            style={{ color: q.pct != null ? ARC_STROKE[q.level] : "transparent" }}
          >
            {q.hasData && (
              <path d={capArc} pathLength={100} className="edge-arc-track" />
            )}
            {q.hasData && q.pct != null && (
              <path
                d={capArc}
                pathLength={100}
                className="edge-arc"
                style={{ strokeDasharray: `${pctClamped} 100` }}
                stroke={ARC_STROKE[q.level]}
              />
            )}
          </g>
          {/* 告警沿形光晕（形内）：drop-shadow 随弧生成后被裁进弓形——出错红/
              采集异常琥珀逐参数照搬原 rim 光晕；线本体（.edge-rim，形外全长
              勾边）在本体层负责轮廓，光与线分居裁剪内外 */}
          <path d={capArc} className="edge-rim-glow" />
        </g>
        {/* 轮廓光弧本体：贴形全长勾边（不裁剪——形外半边勾边保轮廓完整）；
            告警态只变色不带光，光晕走上方裁剪内 .edge-rim-glow */}
        <path d={capArc} className="edge-rim" />
      </svg>
      <div className={`edge-pills${mirror ? " edge-pills-right" : ""}`} data-tauri-drag-region>
        {segments.map((seg) => (
          <span
            key={seg.agent}
            className={`edge-pill st-${seg.state}`}
            style={{ background: agentColor(seg.agent) }}
            data-tauri-drag-region
          />
        ))}
      </div>
    </div>
  );
}
