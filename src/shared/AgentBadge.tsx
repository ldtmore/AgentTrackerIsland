/**
 * Agent 身份徽章（2026-09-30 徽章统一改版，替代各窗口手搓的色点/色块徽标）：
 * 身份色实底胶囊包裹文字＋顶部微高光；字色按底色亮度自动黑/白——
 * 用户自定义深色身份色时不再黑底黑字；gray 态用于设置页停用卡。
 * 语义规则（02-DESIGN 展示规范延伸）：圆点＝状态，色块徽章＝身份，
 * 身份色永不兼职表达状态；名称口径由调用方决定——
 * 紧凑位（岛卡片/表格/图例）传短名，宽敞位（设置页/抽屉/筛选菜单）传全名
 */
import type { CSSProperties } from "react";
import { agentColor } from "./types";
import "./agentbadge.css";

/** sRGB 相对亮度（WCAG 公式）；非 hex 输入按亮色处理（深字兜底）——
 *  agentColor 恒出合法 hex，仅防御 */
export function badgeLuminance(hex: string): number {
  const m = /^#([0-9a-f]{6})$/i.exec(hex.trim());
  if (!m) return 1;
  const n = parseInt(m[1], 16);
  const f = (v: number) => {
    v /= 255;
    return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
  };
  return 0.2126 * f((n >> 16) & 255) + 0.7152 * f((n >> 8) & 255) + 0.0722 * f(n & 255);
}

/** 徽章双色（2026-10-03 审查修复）：白字 #f8fafc 与深字 #0b0d10 的对比度
 *  分别实算取较大者。原单阈值 0.42 高估了白字可达区间——11 个默认身份色中
 *  6 个（kimi/qwen/mimo/copilot/gemini/openclaw）白字仅 2.2~3.6:1 不达标；
 *  且黑白对比度曲线交点处任一选择都只有 ~4.3:1，任何单阈值都存在窄带两不
 *  达标，双算严格最优（10.5px/700 小字按 WCAG 需 4.5:1） */
export function badgeTextColor(bg: string): string {
  const WHITE = "#f8fafc";
  const DARK = "#0b0d10";
  const l = badgeLuminance(bg);
  const cWhite = (badgeLuminance(WHITE) + 0.05) / (l + 0.05);
  const cDark = (l + 0.05) / (badgeLuminance(DARK) + 0.05);
  return cWhite >= cDark ? WHITE : DARK;
}

export default function AgentBadge({
  agent,
  name,
  size = "md",
  gray = false,
  colorOverride,
  title,
}: {
  /** Agent id（默认按它取身份色；colorOverride 优先——设置页实时预览用） */
  agent: string;
  /** 徽章文字：紧凑位传短名（agentShortName）、宽敞位传全名（agentFullName） */
  name: string;
  /** md=紧凑位（岛卡片/表格，10.5px）；lg=宽敞位（设置页/抽屉/菜单，11.5px） */
  size?: "md" | "lg";
  /** 灰阶态（设置页停用/未启用卡）：不输出内联底色，由 CSS 类接管 */
  gray?: boolean;
  /** 覆盖身份色（设置页拖动取色器的即时预览，落库前 agentColor 还没变） */
  colorOverride?: string;
  /** 悬浮提示（建议传全名，兜底徽章内文字被截断的场景） */
  title?: string;
}) {
  const color = colorOverride ?? agentColor(agent);
  const style: CSSProperties = gray
    ? {}
    : {
        background: `linear-gradient(180deg, rgba(255,255,255,.14), rgba(255,255,255,0) 50%), ${color}`,
        color: badgeTextColor(color),
      };
  return (
    <span
      className={`agt-badge agt-badge-${size}${gray ? " agt-badge-gray" : ""}`}
      style={style}
      title={title}
    >
      {name}
    </span>
  );
}
