/**
 * 单色 SVG 图标集（2026-09-18 展示改造 G3）：
 * 一律 currentColor 继承文字色、12px、线性风格——禁用彩色 emoji
 * （Windows WebView2 渲染彩色 emoji，会破坏暗色科技风）。
 * 语义：芯片 = 模型，文件夹 = 项目，三角警示 = 采集异常，感叹号 = 出错
 */

/** 基础属性：12px 视框，随文字颜色 */
const BASE = {
  width: 12,
  height: 12,
  viewBox: "0 0 24 24",
  fill: "none",
  stroke: "currentColor",
  strokeWidth: 2,
  strokeLinecap: "round" as const,
  strokeLinejoin: "round" as const,
  "aria-hidden": true,
};

/** 芯片（模型徽标前缀） */
export function ChipIcon() {
  return (
    <svg {...BASE} className="ico">
      <rect x="6" y="6" width="12" height="12" rx="2" />
      <rect x="10" y="10" width="4" height="4" />
      <path d="M9 2v3M15 2v3M9 19v3M15 19v3M2 9h3M2 15h3M19 9h3M19 15h3" />
    </svg>
  );
}

/** 文件夹（项目徽标前缀） */
export function FolderIcon() {
  return (
    <svg {...BASE} className="ico">
      <path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
    </svg>
  );
}

/** 三角警示（采集异常/降级提示） */
export function WarnIcon() {
  return (
    <svg {...BASE} className="ico ico-warn">
      <path d="M12 3 2.5 20h19z" />
      <path d="M12 10v4M12 17.5v.5" />
    </svg>
  );
}

/** 感叹号（隐藏态出错分段叠加） */
export function BangIcon() {
  return (
    <svg {...BASE} className="ico">
      <path d="M12 4v10M12 19v.5" />
    </svg>
  );
}

/** 柱状图（报表入口）：三根立柱 + 无基线，小尺寸下比带轴图表更易辨识 */
export function ReportIcon() {
  return (
    <svg {...BASE} className="ico">
      <path d="M6 20v-6M12 20V4M18 20v-9" />
    </svg>
  );
}

/** 列表（会话中心入口）：三行条目线，行首圆点示意逐条会话 */
export function SessionsIcon() {
  return (
    <svg {...BASE} className="ico">
      <path d="M8 6h13M8 12h13M8 18h13" />
      <path d="M3.5 6v.5M3.5 12v.5M3.5 18v.5" />
    </svg>
  );
}

/** 胶囊（灵动岛显隐切换项）：横向圆角胶囊即岛的收缩态形态 */
export function IslandIcon() {
  return (
    <svg {...BASE} className="ico">
      <rect x="3" y="8" width="18" height="8" rx="4" />
    </svg>
  );
}

/** 齿轮（设置入口）：中圈 + 六向齿，线性小尺寸下比实心齿圈更清晰 */
export function GearIcon() {
  return (
    <svg {...BASE} className="ico">
      <circle cx="12" cy="12" r="3.2" />
      <path d="M12 2.8v3M12 18.2v3M2.8 12h3M18.2 12h3M5.2 5.2l2.1 2.1M16.7 16.7l2.1 2.1M18.8 5.2l-2.1 2.1M7.3 16.7l-2.1 2.1" />
    </svg>
  );
}

/** 信息圈（关于入口） */
export function InfoIcon() {
  return (
    <svg {...BASE} className="ico">
      <circle cx="12" cy="12" r="9" />
      <path d="M12 11v5M12 8v.5" />
    </svg>
  );
}

/** 电源（退出）：竖线 + 开口圆弧 */
export function PowerIcon() {
  return (
    <svg {...BASE} className="ico">
      <path d="M12 3v8" />
      <path d="M6.3 6.5a8 8 0 1 0 11.4 0" />
    </svg>
  );
}
