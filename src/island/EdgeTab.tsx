/**
 * 贴边隐藏态标签（M1-6 立项，2026-10-08 左右侧「弓形＋药丸」改版）：
 * 独立微 UI，非胶囊截取——**它是「胶囊本人被屏幕边缘遮住大半后露出的局部」**：
 * 左右=端帽切片（真实圆头弓形），顶部=底部切片（横条，所有者的胶囊遮蔽模型）。
 * 设计语言与位置无关（状态统一原则）：
 * - **颜色 = Agent 身份**，顶部横条分段 / 左右竖向药丸点列——同一套
 *   "分段＋亮度动效"语言按形态适配（批次三删除旧扇形楔块：20px 宽的饼图
 *   隐喻信息带宽为零；2026-10-08 竖条改药丸：密度恒定＋中轴放置，不再被
 *   容器曲率裁切成碎条）；
 * - 状态用亮度/动效表达：工作中=全亮慢呼吸 / 等待=快闪 / 出错=红圈描边+快闪 /
 *   空闲=55% 暗淡 / 离线=近隐没；全部会话离线的 Agent 不渲染分段（E2）；
 *   三处逐参数一致（同一 keyframes/同一透明度档），出错「！」是顶部宽敞位
 *   的冗余增强，红圈才是三处同义的语义本体（左右药丸用外描边保身份色完整）；
 * - **额度线 = 隐藏态自己的边缘轮廓**：左右贴端帽圆弧（与容器轮廓同一条
 *   弧线），顶部贴底缘轮廓线（SVG path 按实际 peek 宽精确生成）——
 *   dasharray=已用%，点亮的是同一粒胶囊的不同边缘，与胶囊/面板/托盘同源
 *   选择器（tensestAccount）；F5：展示集有实例但无可用数据 → 空轮廓可见；
 *   余额告急 → 满线 danger（微 UI 无金额位，矩阵规定的降级表达）；
 * - 采集异常 → 轮廓光转琥珀（F3，与出错红同语言，状态不再因位置沉默）；
 * - 上岛深度（2026-10-08）：分段只渲染拖拽序前 N 个（默认 5、上限 6＝
 *   药丸列满配容量）；过滤仅在本渲染层——胶囊文案/容器描边的全局聚合
 *   不受限，界外 Agent 出错仍由红边＋文案兜底。
 *
 * ⚠ 本区域明确不做 tooltip（2026-09-18 与用户确认的伪需求，勿再实现）：
 * 根容器的 onMouseEnter 在鼠标进入隐藏态瞬间触发 island_peek 滑入，
 * 本组件随即卸载换成胶囊渲染（App.tsx 悬停激活链路）——任何悬停提示
 * 都没有稳定展示时机。额度与状态详情由滑入后的胶囊 tooltip 承接
 */
import type { IslandSnapshot, SessionState, Thresholds } from "../shared/types";
import { agentColor, quotaLevel, tensestAccount } from "../shared/types";
import { agentLimit, orderAgentIds } from "../shared/agentOrder";
import { BangIcon } from "../shared/icons";

/** 顶部贴边条高（与 Rust PEEK_TOP_H 同值同义；轮廓路径端弧半径=条高，半圆收尾） */
const EDGE_TOP_H = 14;

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

/** 顶部切片的下缘轮廓路径（额度线即轮廓本身）：左上角起笔→左端弧→底边直线→
 *  右端弧收笔。端弧半径=条高（quarter circle），与容器 border-radius 同一几何；
 *  path 按实际 peek 宽精确生成（无拉伸无变形），dasharray 沿轮廓推进＝「胶囊底缘
 *  被点亮到已用% 处」——与左右端帽弧同一套「线即边缘」语言。
 *  ⚠ 端弧 sweep=0（弧心在端的上方内侧：左端 (r,0)、右端 (w-r,0)），弧经靠下
 *  象限贴住条的左下/右下圆角剪影；sweep=1 会画成贴内上角的对角斜线
 *  （2026-09-28 所有者截图实認的返工点，两枚 flag 勿再翻转） */
function topOutlinePath(w: number): string {
  const r = EDGE_TOP_H;
  return `M 0 0 A ${r} ${r} 0 0 0 ${r} ${r} L ${w - r} ${r} A ${r} ${r} 0 0 0 ${w} 0`;
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
  /** 顶部贴边隐藏态的标签宽度（Rust peek_top_width：胶囊公式常数减半，恒 2:1）。
   *  窗口保持全宽，标签以 CSS 居中呈现窄条；两侧透明区域由后端动态鼠标穿透放行点击 */
  peekW: number;
}) {
  const error = snap?.island === "any_error" || snap?.quota_exhausted === true;
  // 采集异常（F3）：容器琥珀描边；与出错同时发生时红描边优先（更严重的语义独占边框）
  const degraded = snap?.degraded === true && !error;
  const cls = `edge-tab edge-tab-${edge}${error ? " edge-error" : ""}${degraded ? " edge-degraded" : ""}`;
  const segments = agentSegments(snap);
  const q = quotaSignal(snap, thresholds);
  const pctClamped = q.pct != null ? Math.min(100, Math.max(0, q.pct)) : 0;

  // 顶部贴边：胶囊的底部切片（横条），Agent 身份色横条分段＋底缘轮廓额度线
  if (edge !== "left" && edge !== "right") {
    return (
      <div className={cls} style={{ width: peekW }} data-tauri-drag-region>
        {segments.map((seg) => (
          // 出错分段叠加「！」（E4）：纯色微 UI 上唯一的文字级符号
          <span key={seg.agent} className="edge-seg-wrap" data-tauri-drag-region>
            <span
              className={`edge-seg st-${seg.state}`}
              style={{ background: agentColor(seg.agent) }}
              data-tauri-drag-region
            />
            {seg.state === "error" && (
              <span className="edge-bang" data-tauri-drag-region>
                <BangIcon />
              </span>
            )}
          </span>
        ))}
        {q.hasData && (
          // 额度线＝切片下缘轮廓本身（含两端端弧），path 按实际 peek 宽生成；
          // F5：pct=null（暂无数据）仅画空轮廓，明示「有实例但暂无数据」
          <svg
            className="edge-svg"
            viewBox={`0 0 ${peekW} ${EDGE_TOP_H}`}
            preserveAspectRatio="none"
            data-tauri-drag-region
          >
            <path
              d={topOutlinePath(peekW)}
              pathLength={100}
              className="edge-arc-track edge-arc-round"
            />
            {q.pct != null && (
              // dasharray 经 style 下发（CSS 可观察）：用量变化时轮廓线平滑推进；
              // color 供 .edge-arc 的 drop-shadow(currentColor) 取档位色
              <path
                d={topOutlinePath(peekW)}
                pathLength={100}
                className="edge-arc edge-arc-round"
                style={{ strokeDasharray: `${pctClamped} 100`, color: ARC_STROKE[q.level] }}
                stroke={ARC_STROKE[q.level]}
              />
            )}
          </svg>
        )}
      </div>
    );
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
        {/* 弓形本体：真实端帽切片的近黑填充（形状真源在 SVG——矩形 div 画不出
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
