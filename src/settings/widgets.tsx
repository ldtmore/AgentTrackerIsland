/**
 * 设置页基础组件与常量（2026-10-03 四轮审查拆分：自 Settings.tsx 纯移动，
 * JSX 与行为逐字节不变）——分区卡/设置行/开关/分段控件/厂商选择器等本页积木。
 * 独立成模块后设置页各分区组件（AgentSection/QuotaSection/ProviderFormModal）
 * 可共用，不再挤在单一巨型组件里。
 */
import { useEffect, useRef, useState } from "react";
import Tip from "../shared/Tip";
import { ChevronIcon, InfoIcon } from "../shared/icons";
import type { KindEntryView } from "../shared/providerTypes";
import { kindEntryKey } from "../shared/providerTypes";

/** Claude 订阅档位（M3-12）：auto＝近 8 天用量 P90 自动探测限额（Maciek 做法），
 *  pro/max5/max20＝社区测算硬编码限额（Maciek plans.py） */
export const CLAUDE_PLAN_OPTIONS: { label: string; value: string }[] = [
  { label: "自动探测", value: "auto" },
  { label: "Pro", value: "pro" },
  { label: "Max 5x", value: "max5" },
  { label: "Max 20x", value: "max20" },
];

/** 数据保留时长选项（天），按时长降序；12 个月 = 365 天，与后端"未设置默认保留 1 年"一致 */
export const CLEANUP_OPTIONS: { label: string; days: number }[] = [
  { label: "12 个月", days: 365 },
  { label: "6 个月", days: 180 },
  { label: "3 个月", days: 90 },
  { label: "1 个月", days: 30 },
  { label: "15 天", days: 15 },
  { label: "7 天", days: 7 },
  { label: "1 天", days: 1 },
];

/** 默认保留时长（12 个月；与后端清理默认值 365 天一致） */
export const CLEANUP_DEFAULT_DAYS = 365;

/** 托盘左键动作（与 Rust 端 tray_left_action 同源约定）：none=无操作（默认档） */
export type TrayLeftAction = "none" | "toggle" | "menu";
export const TRAY_LEFT_DEFAULT: TrayLeftAction = "none";

/** 岛显隐全局快捷键的固定组合（07-UX 2.3，与 Rust 端 tray.rs 的
 *  HOTKEY_ISLAND_COMBO 同源约定）：本期只有「关闭／默认组合」两档，
 *  自定义组合输入列后续增强档 */
export const HOTKEY_ISLAND_DEFAULT = "ctrl+alt+i";

/** 额度与凭据分区的 DOM 锚点（额度页空态「去设置添加」滚动直达用，M3-5） */
export const QUOTA_SECTION_ID = "st-section-quotas";

/** 锚点条高度（px）：settings.css 的 --st-nav-h 与此同源，改动需两侧同步 */
export const NAV_HEIGHT_PX = 42;

/** 顶部锚点条分区定义（阶段二）：顺序即页面分区顺序，id 与各 Section 的 DOM 锚点一致 */
export const NAV_SECTIONS: { id: string; label: string }[] = [
  { id: "st-section-island", label: "灵动岛" },
  { id: "st-section-agents", label: "Agent 监控" },
  { id: QUOTA_SECTION_ID, label: "额度与凭据" },
  { id: "st-section-general", label: "通用" },
  { id: "st-section-data", label: "数据与维护" },
];

/** 平滑滚动到指定分区顶；尊重系统「减弱动态效果」设置（红线⑤延伸） */
export function scrollToSection(id: string) {
  const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  document
    .getElementById(id)
    ?.scrollIntoView({ behavior: reduce ? "auto" : "smooth", block: "start" });
}

/** 顶部锚点条（阶段二）：sticky 钉在滚动容器顶，点击平滑滚到分区；
 *  当前分区高亮由父组件的 scroll-spy 计算（aria-current 供读屏跟随） */
export function SectionNav({ current, onJump }: { current: string; onJump: (id: string) => void }) {
  return (
    <nav className="st-nav" aria-label="设置分区导航">
      {NAV_SECTIONS.map((s) => (
        <button
          key={s.id}
          type="button"
          className={`st-nav-item${current === s.id ? " st-nav-on" : ""}`}
          aria-current={current === s.id ? "true" : undefined}
          onClick={() => onJump(s.id)}
        >
          {s.label}
        </button>
      ))}
    </nav>
  );
}

/** 分区卡片：主标题 + 副标题同行（主/副标题关系），下方为设置行列表。
 *  id 可选：供跨窗口跳转滚动定位（如额度页直达额度分区） */
export function Section({
  title,
  desc,
  id,
  children,
}: {
  title: string;
  desc?: string;
  id?: string;
  children: React.ReactNode;
}) {
  return (
    <section className="st-section" id={id}>
      <div className="st-sec-head">
        <div className="st-sec-title">{title}</div>
        {/* 副标题窄窗 ellipsis 截断兜底（C6）：悬浮给全文 */}
        {desc && <div className="st-sec-desc" title={desc}>{desc}</div>}
      </div>
      <div className="st-sec-body">{children}</div>
    </section>
  );
}

/** 设置行：左 = 固定标题 + 固定描述（+可选 ⓘ 更多说明），右 = 控件；
 *  error 就近显示在本行下方 */
export function Row({
  title,
  desc,
  tip,
  error,
  tall,
  children,
}: {
  title: string;
  desc?: string;
  /** ⓘ 悬浮提示内容（I2 改造）：从描述里挪出来的开启/关闭分支与技术细节，
   *  悬停行尾 ⓘ 图标显示——信息只挪不丢 */
  tip?: string;
  error?: string;
  /** tall：宽控件（输入框）放到文字下方独占一行 */
  tall?: boolean;
  children?: React.ReactNode;
}) {
  return (
    <div className="st-row-item">
      <div className="st-row-main">
        <div className="st-row-text">
          <div className="st-row-title">
            {title}
          </div>
          {desc && (
            <div className="st-row-desc">
              {desc}
              {tip && (
                <Tip content={tip}>
                  <span className="st-row-tip" aria-label="更多说明">
                    <InfoIcon />
                  </span>
                </Tip>
              )}
            </div>
          )}
        </div>
        {!tall && children != null && <div className="st-row-control">{children}</div>}
      </div>
      {tall && children != null && (
        <div className="st-row-control st-row-control-tall">{children}</div>
      )}
      {error && <div className="st-row-error">{error}</div>}
    </div>
  );
}

/** 滑动开关：状态由开/关位置表达，标题文案保持固定；small 为行内小号变体（Agent 行精确开关），disabled 用于 busy/前置条件不满足 */
export function Switch({
  checked,
  onChange,
  disabled,
  title,
  small,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
  title?: string;
  small?: boolean;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      className={`st-switch${small ? " st-switch-sm" : ""}${checked ? " st-switch-on" : ""}`}
      disabled={disabled}
      title={title}
      onClick={() => onChange(!checked)}
    >
      <span className="st-switch-thumb" />
    </button>
  );
}

/** 分段选择（泛化）：点击即切换，供主题三档 / 托盘左键三档 / 凭据来源两档复用；
 *  聚焦后支持方向键循环切换（←↑ 上一个、→↓ 下一个），radio group 语义与键盘行为对齐 */
export function Segmented<T extends string>({
  value,
  onChange,
  options,
  ariaLabel,
}: {
  value: T;
  onChange: (v: T) => void;
  options: { value: T; label: string }[];
  ariaLabel: string;
}) {
  const groupRef = useRef<HTMLDivElement>(null);

  /** 方向键循环：受控值经 onChange 切换，焦点随后跟到新选中项（roving 简化版：
   *  各项仍可 Tab 逐个到达，方向键在已聚焦项上滚动选项） */
  const onKeyDown = (e: React.KeyboardEvent) => {
    const dir =
      e.key === "ArrowLeft" || e.key === "ArrowUp"
        ? -1
        : e.key === "ArrowRight" || e.key === "ArrowDown"
          ? 1
          : 0;
    if (dir === 0) return;
    e.preventDefault();
    const idx = options.findIndex((o) => o.value === value);
    onChange(options[(idx + dir + options.length) % options.length].value);
    requestAnimationFrame(() => {
      groupRef.current
        ?.querySelector<HTMLButtonElement>('[aria-checked="true"]')
        ?.focus();
    });
  };

  return (
    <div
      ref={groupRef}
      className="st-seg"
      role="radiogroup"
      aria-label={ariaLabel}
      onKeyDown={onKeyDown}
    >
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          role="radio"
          aria-checked={value === o.value}
          /* roving tabindex（2026-10-03 审查修复）：仅选中项进 Tab 序——原先
             全项可 Tab，键盘要逐个跳过整组；方向键循环已有（上方 onKeyDown） */
          tabIndex={value === o.value ? 0 : -1}
          className={value === o.value ? "st-seg-item st-seg-on" : "st-seg-item"}
          onClick={() => onChange(o.value)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

/** 步进器（2026-10-08「在岛展示前 N 个」）：−/＋ 步进小整数，边界禁用；
 *  焦点在按钮上时 ←/↑ −1、→/↓ ＋1（与 Segmented 的键盘支持哲学对齐）。
 *  为 1~6 这类连续小整数域设计——分段控件档位过宽、滑块选值不精确，均不适配 */
export function Stepper({
  value,
  min,
  max,
  onChange,
  ariaLabel,
}: {
  value: number;
  min: number;
  max: number;
  onChange: (v: number) => void;
  ariaLabel: string;
}) {
  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
      e.preventDefault();
      if (value > min) onChange(value - 1);
    } else if (e.key === "ArrowRight" || e.key === "ArrowDown") {
      e.preventDefault();
      if (value < max) onChange(value + 1);
    }
  };
  return (
    <div className="st-stepper" role="group" aria-label={ariaLabel} onKeyDown={onKeyDown}>
      <button
        type="button"
        className="st-stepper-btn"
        aria-label="减少"
        disabled={value <= min}
        onClick={() => onChange(Math.max(min, value - 1))}
      >
        −
      </button>
      {/* polite 播报数值变化（读屏跟随步进）；min-width 防位数变化时控件抖动 */}
      <span className="st-stepper-val" aria-live="polite">
        {value}
      </span>
      <button
        type="button"
        className="st-stepper-btn"
        aria-label="增加"
        disabled={value >= max}
        onClick={() => onChange(Math.min(max, value + 1))}
      >
        ＋
      </button>
    </div>
  );
}

/** 眼睛图标（off=true 画斜线，表示当前隐藏中）。
 *  2026-10-03 审查归一：15px→12px，回规全项目图标统一尺寸（此前唯一非 12px 图标） */
export function EyeIcon({ off }: { off: boolean }) {
  return (
    <svg
      width="12"
      height="12"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d="M1 12s4-7 11-7 11 7 11 7-4 7-11 7S1 12 1 12z" />
      <circle cx="12" cy="12" r="3" />
      {off && <line x1="4" y1="4" x2="20" y2="20" />}
    </svg>
  );
}

/** 厂商可搜索选择器（弹窗用）：2026-10-10 双站变体机制改版——数据源为变体
 *  展开后的条目清单（双站厂商两条成对戴徽标，单站一条无徽标）；条目两行结构
 *  （主行官方定名＋站别徽标，副行「俗名 · 域名」）。搜索三域匹配（条目名/
 *  别名表/域名）＋归一化（小写去空格点横线，zai 命中 Z.ai）——别名永不上屏，
 *  俗名与域名只出现在副行弱化位。交互与 shared/SearchSelect 同模式
 *  （点外关闭/Enter 选首项/关键字过滤），无"全部"档 */
export function KindPicker({
  entries,
  value,
  onChange,
}: {
  entries: KindEntryView[];
  /** 选中条目键（kindEntryKey 组合键：kind_id 或 kind_id:variant_key） */
  value: string;
  onChange: (e: KindEntryView) => void;
}) {
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState("");
  const ref = useRef<HTMLDivElement>(null);

  // 点击组件外部关闭浮层
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [open]);

  // 归一化匹配：z.ai→zai、moonshot.ai→moonshotai，用户输入无符号缩写也能命中
  const normKw = q
    .trim()
    .toLowerCase()
    .replace(/[\s._\-/:]/g, "");
  const filtered = normKw
    ? entries.filter((e) =>
        [e.name, e.alt_name, e.domain, e.kind_id, ...e.aliases].some((t) =>
          t.toLowerCase().replace(/[\s._\-/:]/g, "").includes(normKw),
        ),
      )
    : entries;
  const current = entries.find((e) => kindEntryKey(e) === value);

  return (
    <div className="st-kindpick" ref={ref}>
      <button
        type="button"
        className={`st-kindpick-btn${open ? " st-kindpick-open" : ""}`}
        onClick={() => {
          setOpen(!open);
          setQ(""); // 每次打开重置搜索词
        }}
        aria-label="选择供应商"
      >
        <i className="st-chip-dot" style={{ background: current?.color }} />
        <span className="st-kindpick-text">{current?.name ?? "选择供应商"}</span>
        {current?.badge && <span className="st-kind-badge">{current.badge}</span>}
        <span className="st-kindpick-caret"><ChevronIcon dir="down" /></span>
      </button>
      {open && (
        <div className="st-kindpick-pop">
          <input
            className="st-kindpick-search"
            autoFocus
            value={q}
            placeholder="输入关键字过滤（支持智谱 / zai / 月暗 / ds…）"
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") {
                // Esc 只关浮层，不冒泡到弹窗的"Esc 关弹窗"监听
                e.stopPropagation();
                setOpen(false);
              }
              if (e.key === "Enter" && filtered.length > 0) {
                onChange(filtered[0]);
                setOpen(false);
              }
            }}
          />
          <div className="st-kindpick-list">
            {filtered.map((e) => (
              <button
                key={kindEntryKey(e)}
                type="button"
                className={`st-kindpick-opt${kindEntryKey(e) === value ? " on" : ""}`}
                /* 定名＋凭据指引分行（C7 沿用）：名字截断时悬浮看不到全名；
                   副行域名进悬浮（双站用户核对请求目标） */
                title={`${e.name}${e.domain ? `（${e.domain}）` : ""}\n${e.cred_hint}`}
                onClick={() => {
                  onChange(e);
                  setOpen(false);
                }}
              >
                <i className="st-chip-dot" style={{ background: e.color }} />
                <span className="st-kindpick-item">
                  <span className="st-kindpick-main">
                    {e.name}
                    {e.badge && <span className="st-kind-badge">{e.badge}</span>}
                  </span>
                  {(e.alt_name || e.domain) && (
                    <span className="st-kindpick-sub">
                      {[e.alt_name, e.domain].filter(Boolean).join(" · ")}
                    </span>
                  )}
                </span>
              </button>
            ))}
            {filtered.length === 0 && (
              <div className="st-kindpick-none">
                无匹配厂商，可用「自定义中转」接入任意 OpenAI 兼容站点
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

/** Enter 直接提交（失焦提交的快捷路径） */
export function blurOnEnter(e: React.KeyboardEvent<HTMLInputElement>) {
  if (e.key === "Enter") (e.target as HTMLInputElement).blur();
}
