/**
 * 报表页（M1-R1 重构；M1-10 瘦身：会话明细迁往独立会话窗口，本页回归纯聚合分析；
 * 2026-09-24 报表改造：汇总卡加环比与 ⓘ 口径提示、活跃项目换缓存命中率、
 * 额度消耗历史曲线、按模型首字延迟、供应商可筛选、维度条指标切换/展开全部/
 * 散列稳定色、热力图 P95 封顶色阶、手动刷新＋数据截至）：
 * 汇总卡 / 趋势（粒度随范围自动：今日→小时、7~30 天→日、90 天→周、全部→月）/
 * Agent·项目·模型·供应商·首字延迟维度条（点击联动全局筛选）/
 * 周×小时热力图。数据来自本地 SQLite，Rust 侧单命令整页快照（各图口径一致）；
 * 时间口径为本机时区；筛选变更全量重拉（本地查询毫秒级）
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { save } from "@tauri-apps/plugin-dialog";
import * as echarts from "echarts/core";
import type { EChartsCoreOption } from "echarts/core";
import { BarChart, HeatmapChart, LineChart } from "echarts/charts";
import {
  GridComponent,
  LegendComponent,
  TooltipComponent,
  VisualMapComponent,
} from "echarts/components";
import { CanvasRenderer } from "echarts/renderers";
import Chart from "./Chart";
import SearchSelect from "../shared/SearchSelect";
import Tip from "../shared/Tip";
import Toast, { type ToastData } from "../shared/Toast";
import EmptyState from "../shared/EmptyState";
import { ArrowIcon, ChevronIcon, InfoIcon, ReportIcon, WarnIcon } from "../shared/icons";
import { fmtDTFull, fmtMs, fmtUsd, tail } from "../shared/format";
import {
  accountLabel,
  fmtTokens,
  fmtDuration,
  agentColor,
  hashColor,
  errorReason,
  windowLabel,
} from "../shared/types";
import { agentFullName, agentShortName } from "../shared/sessionDisplay";
import { orderAgentIds, parseAgentOrder, setAgentOrder } from "../shared/agentOrder";
import { useTheme } from "../shared/theme";
import { useAgentColors } from "../shared/useAgentColors";
import "./report.css";

// 按需注册用到的图表与组件（01-RESEARCH §9，减小 bundle）
echarts.use([
  BarChart,
  HeatmapChart,
  LineChart,
  GridComponent,
  LegendComponent,
  TooltipComponent,
  VisualMapComponent,
  CanvasRenderer,
]);

// ===== 与 Rust store 报表结构对应的类型 =====

/** 汇总卡（范围＋筛选下的全量口径；duration/ttft 为 null 表示该范围无时长数据） */
interface SummaryStats {
  total_tokens: number;
  calls: number;
  sessions: number;
  errors: number;
  duration_ms: number | null;
  ttft_avg_ms: number | null;
  reasoning_tokens: number;
  billable_tokens: number;
  /** 输入 token 合计（缓存命中率分母之一） */
  input_total: number;
  /** 缓存读 token 合计（缓存命中率分子） */
  cache_read_total: number;
  /** 估算成本（美元；null=范围内无任何模型可计价，M3-1） */
  est_cost: number | null;
  /** 缺单价的模型个数（可用单价覆盖补齐） */
  missing_price_count: number;
}

/** 单维度分组行（token 总量 + 调用次数 + 估算成本；首字延迟行 total=平均毫秒；错误分布里 total/calls 同为次数、cost 恒 0） */
interface SliceUsage {
  label: string;
  total: number;
  calls: number;
  /** 估算成本（美元；按当前单价、价格变动不追溯） */
  cost: number;
}

/** 趋势行（时间桶 + 四项用量 + 思考 + 次数 + 生成时长 + 估算成本，指标切换免回查） */
interface TrendRow {
  bucket: string;
  input: number;
  output: number;
  cache_read: number;
  cache_creation: number;
  reasoning: number;
  calls: number;
  duration_ms: number | null;
  /** 该桶估算成本（美元） */
  cost: number;
}

/** 趋势维度切片（#9a）：时间桶×维度值的 token 合计 */
interface TrendSlice {
  bucket: string;
  label: string;
  total: number;
}

/** 热力图单元（token 与次数双指标） */
interface HeatCell {
  weekday: number; // 0=周日
  hour: number;
  total: number;
  calls: number;
}

/** 额度历史采样点（约 5 分钟一条，Rust 侧超量抽稀） */
interface QuotaPoint {
  provider: string;
  /** 实例归属（2026-09-29 审查修复 #8：同厂商多实例各自一条线不混串）；
   *  null=0007 之前的存量快照（按"默认实例"分线） */
  account_id: string | null;
  window_kind: string;
  fetched_at: number;
  used_percent: number;
}

/** 额度曲线实例标签（quota_accounts 下发；曲线序列名的解析依据） */
interface QuotaAccountLabel {
  account_id: string;
  provider: string;
  kind_name: string;
  alias: string | null;
}

/** 筛选下拉选项（projects 中空串=未知项目；providers 中 "unknown"=缺失供应商） */
interface FilterOptions {
  agents: string[];
  projects: string[];
  models: string[];
  providers: string[];
  quota_accounts: QuotaAccountLabel[];
}

/** 整页快照 */
interface ReportSnapshot {
  summary: SummaryStats;
  /** 上一等长周期汇总（环比基准）；all 档为 null 不比 */
  prev_summary: SummaryStats | null;
  trend: TrendRow[];
  /** 趋势维度切片（#9a）：token 指标下「按 Agent／按模型」堆叠切换的数据源 */
  trend_agent_slices: TrendSlice[];
  trend_model_slices: TrendSlice[];
  by_agent: SliceUsage[];
  by_project: SliceUsage[];
  by_model: SliceUsage[];
  by_provider: SliceUsage[];
  by_error: SliceUsage[];
  ttft_by_model: SliceUsage[];
  quota_curve: QuotaPoint[];
  heatmap: HeatCell[];
  options: FilterOptions;
}

/** 范围档（与 Rust build_filter 白名单同步） */
const RANGES = [
  { key: "today", label: "今日" },
  { key: "7d", label: "近 7 天" },
  { key: "30d", label: "近 30 天" },
  { key: "90d", label: "近 90 天" },
  { key: "all", label: "全部" },
];

const WEEKDAYS = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];

/** 堆叠分类（颜色见组件内 seriesColors 双主题分档——四轮审查：颜色自常量移出）。
 *  2026-10-03 审查修复：「输出」原绿与全局强调绿同值——同屏语境里图例绿
 *  会被读作强调/状态语义，换 teal 拉开（缓存写琥珀为分类标签所约束，
 *  有文字锚定不兼职告警语义，保留） */
const STACK = [
  { key: "input" as const, name: "输入" },
  { key: "output" as const, name: "输出" },
  { key: "cache_read" as const, name: "缓存读" },
  { key: "cache_creation" as const, name: "缓存写" },
];

/** 趋势桶显示标签：今日→"HH:00"、7/30 天与 90 天→"MM-DD"、全部→"YYYY-MM" */
function bucketLabel(b: string, range: string): string {
  if (range === "today") return b.slice(11);
  if (range === "all") return b;
  return b.slice(5);
}

/** 年度热力图（#24）：近 365 天逐日 token 格子（GitHub 风格）——
 *  7 行（周几）×53 列（周）；色阶按非零日的分位分 4 档（P25/P50/P75/P95），
 *  悬浮显示日期与用量。恒全量口径不随筛选联动（评审后暂缓，待口径重设计）。
 *  与周×小时热力图互补 */
function YearHeatmap({ refreshTick }: { refreshTick: number }) {
  const [rows, setRows] = useState<[string, number, number][] | null>(null);
  // 加载失败显式呈现（2026-10-10 审查收口，对齐报表页「失败≠空态」纪律）：
  // 原失败 setRows([]) → 365 格全灰像「全年无用量」，失败被伪装成空数据；
  // 现保留骨架/旧数据＋失败行＋重试
  const [loadError, setLoadError] = useState<string | null>(null);
  // 本地重试计数（与 refreshTick 并列：「刷新」按钮重拉全页，此处只重拉本卡）
  const [retryTick, setRetryTick] = useState(0);
  // 令牌防乱序（四轮审查）：快速连点刷新时后发先至的旧响应会覆盖新数据
  const seq = useRef(0);
  useEffect(() => {
    const cur = ++seq.current;
    invoke<[string, number, number][]>("year_heatmap")
      .then((r) => {
        if (cur === seq.current) {
          setRows(r);
          setLoadError(null);
        }
      })
      .catch((e) => {
        // 失败不再清空 rows（保留骨架/上次数据），仅记录错误显式呈现
        if (cur === seq.current) setLoadError(String(e));
      });
  }, [refreshTick, retryTick]); // 随「刷新」重拉（2026-10-03 审查修复：原先挂载拉一次后不再更新）
  if (rows == null || (rows.length === 0 && loadError != null)) {
    // 加载占位（2026-10-03 审查修复）：此前返回 null，数据到达时整卡突然插入
    // 推挤下方内容跳版——365 格 lv0 灰占位保持卡片高度稳定
    return (
      <section className="rp-card">
        <div className="rp-card-head">
          <div className="rp-card-title">年度用量热力图（近 365 天）</div>
        </div>
        <div className="rp-yearheat" style={{ "--yh-cols": 53 } as React.CSSProperties}>
          {Array.from({ length: 365 }, (_, i) => (
            <i
              key={i}
              className="rp-yh-cell lv0"
              style={{ gridColumn: Math.floor(i / 7) + 1, gridRow: (i % 7) + 1 }}
            />
          ))}
        </div>
        {loadError != null && (
          <div className="rp-yh-legend">
            {`加载失败：${loadError}`}
            <button type="button" className="rp-btn" onClick={() => setRetryTick((t) => t + 1)}>
              重试
            </button>
          </div>
        )}
      </section>
    );
  }
  if (rows.length === 0) return null; // 无数据整卡隐藏（有失败信息时上方分支已接管）
  const byDate = new Map(rows.map((r) => [r[0], [r[1], r[2]]]));
  // 非零日分位（色阶分档）：P25/P50/P75/P95
  const tokens = rows.map((r) => r[1]).filter((t) => t > 0).sort((a, b) => a - b);
  const q = (p: number) => tokens[Math.min(tokens.length - 1, Math.floor(tokens.length * p))] ?? 1;
  const levels = [q(0.25), q(0.5), q(0.75), q(0.95)];
  // 近 365 天格子：从 364 天前到今天，列起点对齐周日（首列前补空位）
  const p2 = (n: number) => String(n).padStart(2, "0");
  const dateKey = (d: Date) => `${d.getFullYear()}-${p2(d.getMonth() + 1)}-${p2(d.getDate())}`;
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const start = new Date(today.getTime() - 364 * 86_400_000);
  const lead = start.getDay(); // 首列前的空位数（对齐周日为首行）
  const cells: { key: string; date: string; tokens: number; calls: number; i: number }[] = [];
  for (let i = 0; i < 365; i++) {
    const d = new Date(start.getTime() + i * 86_400_000);
    const k = dateKey(d);
    const [t, c] = byDate.get(k) ?? [0, 0];
    cells.push({ key: k, date: `${p2(d.getMonth() + 1)}-${p2(d.getDate())}`, tokens: t, calls: c, i: i + lead });
  }
  const levelOf = (t: number) => {
    if (t <= 0) return 0;
    if (t <= levels[0]) return 1;
    if (t <= levels[1]) return 2;
    if (t <= levels[2]) return 3;
    if (t <= levels[3]) return 4;
    return 5;
  };
  // 实际列数（含首列周日补位，53 或 54）经 CSS 变量下发：格子平分卡片宽度铺满
  const cols = Math.floor((364 + lead) / 7) + 1;
  return (
    <section className="rp-card">
      <div className="rp-card-head">
        <div className="rp-card-title">年度用量热力图（近 365 天）</div>
      </div>
      <div className="rp-yearheat" style={{ "--yh-cols": cols } as React.CSSProperties}>
        {cells.map((c) => (
          /* 自绘 Tip（07-UX 3.1）：原生 title 在深色页上白底气泡违和且延迟 ~1s，
              Tip 400ms 且随主题；focusable=false——365 格全部入 Tab 序会把键盘
              导航污染到不可用，信息仅悬停一条通道（原生 title 原也无聚焦通道） */
          <Tip
            key={c.key}
            focusable={false}
            content={`${c.date} · ${c.tokens > 0 ? `${fmtTokens(c.tokens)} token · ${c.calls} 次调用` : "无用量"}`}
          >
            <i
              className={`rp-yh-cell lv${levelOf(c.tokens)}`}
              style={{ gridColumn: Math.floor(c.i / 7) + 1, gridRow: (c.i % 7) + 1 }}
            />
          </Tip>
        ))}
      </div>
      {/* 刷新失败但手上有旧数据（2026-10-10 审查收口）：横幅提示＋保留旧热力图
          ＋本卡独立重试（不动全页 refreshTick） */}
      {loadError != null && (
        <div className="rp-yh-legend">
          刷新失败，展示上次数据
          <button type="button" className="rp-btn" onClick={() => setRetryTick((t) => t + 1)}>
            重试
          </button>
        </div>
      )}
      <div className="rp-yh-legend">
        少
        <i className="rp-yh-cell lv1" />
        <i className="rp-yh-cell lv2" />
        <i className="rp-yh-cell lv3" />
        <i className="rp-yh-cell lv4" />
        <i className="rp-yh-cell lv5" />
        多（按非零日分位分档）
      </div>
    </section>
  );
}

/** 可搜索下拉（筛选用）已抽至 shared/SearchSelect（M1-10：报表页与会话窗口共用） */

/** 指标键（趋势图四档；热力图两档无成本数据；维度条三档无时长数据） */
type MetricKey = "token" | "calls" | "duration" | "cost";

/** 毫秒 → 短格式 "45s"/"12m"/"1.3h"（趋势图 y 轴用，长格式会挤爆轴标签） */
function fmtAxisDur(ms: number): string {
  if (ms < 60_000) return `${Math.round(ms / 1000)}s`;
  if (ms < 3_600_000) return `${Math.round(ms / 60_000)}m`;
  return `${(ms / 3_600_000).toFixed(1)}h`;
}

/** 美元 → 短格式 "$1.2K"/"$3.4M"（趋势成本模式 y 轴用） */
function fmtAxisUsd(v: number): string {
  if (v >= 1_000_000) return `$${(v / 1_000_000).toFixed(1)}M`;
  if (v >= 1_000) return `$${(v / 1_000).toFixed(1)}K`;
  return `$${v.toFixed(v >= 100 ? 0 : 2)}`;
}

/** 供应商显示名：glm → GLM（未知供应商口径值 "unknown" 由调用方映射） */
/** 供应商显示名：注册表中文名优先（2026-09-29 审查修复 #10：原一律
 *  p.toUpperCase() 把 "custom-openai" 显示成 "CUSTOM-OPENAI" 生硬且丢中文）；
 *  注册表外（模型目录归因出的 anthropic/openai 等公司 id）保底大写 */
let kindNames: Map<string, string> = new Map();
/** 供应商品牌色注册表投影（2026-10-03 审查修复）：额度曲线按实例品牌色取色
 *  （原 5h 固定蓝与 Codex 身份色撞值、weekly 与 qwen 撞值，曲线会被误读为
 *  「某 Agent 的线」）；窗口类型改用实线/虚线区分 */
let kindColors: Map<string, string> = new Map();
function providerLabel(p: string): string {
  return kindNames.get(p) ?? p.toUpperCase();
}

/** ⓘ 信息图标（与设置页同款）：标签尾「有更多说明」的功能性标记，悬停出 Tip 气泡 */

/** 环比小字：当前值 vs 上一等长周期（今日 vs 昨天、近 7 天 vs 上一个 7 天…）。
 *  任一端为 null（无数据 / all 档无上期）不显示；上期为 0 显示「上期无数据」；
 *  warnUp=true 时上升标警示色（如出错次数——用量涨跌本身无好坏，保持中性灰） */
function Delta({
  value,
  prev,
  warnUp,
}: {
  value: number | null;
  prev: number | null;
  warnUp?: boolean;
}) {
  if (value == null || prev == null) return null;
  if (prev === 0)
    return value > 0 ? (
      <div className="rp-delta" title="较上一等长周期">
        上期无数据
      </div>
    ) : null;
  const pct = ((value - prev) / prev) * 100;
  if (Math.abs(pct) < 0.5)
    return (
      <div className="rp-delta" title="较上一等长周期">
        持平
      </div>
    );
  const up = pct > 0;
  return (
    <div
      className={`rp-delta${warnUp && up ? " bad" : ""}`}
      title="较上一等长周期（如：今日 vs 昨天、近 7 天 vs 上一个 7 天）"
    >
      <>{up ? <ArrowIcon dir="up" /> : <ArrowIcon dir="down" />} {Math.abs(Math.round(pct))}%</>
    </div>
  );
}

/** 指标切换小按钮组（Token/次数/时长，按图可配）；泛型化以复用给
 * 堆叠维度切换（#9a，取值集与 MetricKey 不同） */
/** ⓘ 两行提示（2026-10-03 悬浮提示优化）：主口径行＋弱色补充说明行——
 *  原先「主口径；补充说明；例外」分号串联一行，语义混排难扫读；
 *  与面板轮播点/历史折叠行气泡同一套主次分级语言。dim 缺省退化为单行 */
function twoTip(main: string, dim?: string): React.ReactNode {
  if (!dim) return main;
  return (
    <div className="tip-breakdown">
      <div>{main}</div>
      <div className="tip-dim">{dim}</div>
    </div>
  );
}

function MetricToggle<K extends string>({
  value,
  onChange,
  options,
  disabled = false,
  disabledTip,
}: {
  value: K;
  onChange: (v: K) => void;
  options: { key: K; label: string }[];
  /** 整组禁用（2026-10-03 布局修复）：恒渲染仅锁交互——条件卸载会让头部
   *  space-between 重排（另一组按钮跳位）；禁用态弱化不消失，位置恒定 */
  disabled?: boolean;
  /** 禁用原因（原生 title 挂组容器；disabled 按钮不派发鼠标事件） */
  disabledTip?: string;
}) {
  return (
    <div className="rp-metric" title={disabled ? disabledTip : undefined}>
      {options.map((o) => (
        <button
          key={o.key}
          className={`rp-metric-btn${value === o.key ? " on" : ""}`}
          disabled={disabled}
          aria-pressed={value === o.key}
          onClick={() => onChange(o.key)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

/**
 * 维度比例条（HTML 自绘，点击条目=全局筛选该维度，再点取消；不占 ECharts 实例）。
 * 2026-09-24 改造：指标跟随面板头切换（Token/次数，排序与条长随之）；
 * 「其余 N 项」可展开全部；颜色默认按名字散列稳定分配（跨筛选不漂移）
 */
function DimBars({
  title,
  items,
  active,
  onToggle,
  colorFn,
  hashTheme,
  fmt,
  fmtVal,
  tipFormat,
  selectable = true,
  sortable = true,
  metric = "token",
}: {
  title: string;
  items: SliceUsage[];
  /** 当前生效的筛选集合（#9b 多选；null=未筛选） */
  active: string[] | null;
  /** 点击行 toggle 该值进/出筛选集合（selectable=false 时不会触发） */
  onToggle: (label: string) => void;
  /** 行颜色（缺省按名字散列稳定取色） */
  colorFn?: (label: string) => string;
  /** 哈希兜底色的主题档（五轮审查）：浅色主题必须传 "light"，否则浅底近隐形 */
  hashTheme: "dark" | "light";
  /** 标签显示转换（如项目路径→尾段；过滤值仍用原值） */
  fmt?: (label: string) => string;
  /** 值列显示转换（token 模式；缺省 fmtTokens） */
  fmtVal?: (total: number) => string;
  /** 悬浮提示转换（缺省按 token/次数通用文案） */
  tipFormat?: (it: SliceUsage, name: string, on: boolean) => string;
  /** 是否可点击筛选（错误分布、首字延迟等特殊口径可关） */
  selectable?: boolean;
  /** 是否随指标切换重排（首字延迟段固定延迟降序，关掉） */
  sortable?: boolean;
  /** 当前指标（面板头统一切换下发） */
  metric?: "token" | "calls" | "cost";
}) {
  const [expanded, setExpanded] = useState(false);
  // 展示顺序：token 模式信任后端降序；calls/cost 模式按对应值重排（sortable=false 的
  // 特殊口径段——如首字延迟——固定后端顺序不重排）
  const ordered = useMemo(() => {
    if (!sortable || metric === "token") return items;
    if (metric === "cost") return [...items].sort((a, b) => b.cost - a.cost);
    return [...items].sort((a, b) => b.calls - a.calls);
  }, [items, sortable, metric]);
  const metricOf = (it: SliceUsage) =>
    metric === "calls" ? it.calls : metric === "cost" ? it.cost : it.total;
  const max = Math.max(1, ...ordered.map(metricOf));
  const shown = expanded ? ordered : ordered.slice(0, 8);
  const valText = (it: SliceUsage) =>
    metric === "calls"
      ? it.calls.toLocaleString()
      : metric === "cost"
        ? fmtUsd(it.cost)
        : fmtVal
          ? fmtVal(it.total)
          : fmtTokens(it.total);
  const tipOf = (it: SliceUsage, name: string, on: boolean) => {
    if (tipFormat) return tipFormat(it, name, on);
    const usage =
      metric === "calls"
        ? `${it.calls.toLocaleString()} 次调用 · ${fmtTokens(it.total)} token`
        : metric === "cost"
          ? `≈${fmtUsd(it.cost)}（按当前单价估算） · ${fmtTokens(it.total)} token`
          : `${fmtTokens(it.total)} token · ${it.calls.toLocaleString()} 次调用`;
    return `${name} · ${usage}${selectable ? `（点击${on ? "取消" : ""}筛选）` : ""}`;
  };
  return (
    <div className="rp-dim">
      <div className="rp-dim-title">{title}</div>
      {shown.length === 0 ? (
        <div className="rp-dim-empty">无数据</div>
      ) : (
        shown.map((it) => {
          const on = selectable && (active?.includes(it.label) ?? false);
          const name = fmt ? fmt(it.label) : it.label;
          return (
            <div
              key={it.label}
              className={`rp-dim-row${on ? " on" : ""}${selectable ? "" : " flat"}`}
              title={tipOf(it, name, on)}
              /* 键盘可达（2026-10-03 审查补充）：可选中行进 Tab 序、Enter/空格切换——
                 对齐设置页 st-agent 卡先例 */
              {...(selectable
                ? {
                    role: "button",
                    tabIndex: 0,
                    // 按压语义补齐（07-UX 3.7）：已声明 role=button 的筛选切换，
                    // 按下状态对读屏可见
                    "aria-pressed": on,
                    onClick: () => onToggle(it.label),
                    onKeyDown: (e: React.KeyboardEvent) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        onToggle(it.label);
                      }
                    },
                  }
                : {})}
            >
              <span className="rp-dim-name" title={name}>
                {name}
              </span>
              <span className="rp-dim-track">
                <span
                  className="rp-dim-fill"
                  style={{
                    width: `${(metricOf(it) / max) * 100}%`,
                    background: colorFn ? colorFn(it.label) : hashColor(it.label, hashTheme),
                  }}
                />
              </span>
              <span className="rp-dim-val">{valText(it)}</span>
            </div>
          );
        })
      )}
      {ordered.length > 8 && (
        <button className="rp-dim-more" onClick={() => setExpanded(!expanded)}>
          {expanded ? <>收起 <ChevronIcon dir="up" /></> : <>展开其余 {ordered.length - 8} 项 <ChevronIcon dir="down" /></>}
        </button>
      )}
    </div>
  );
}

export default function Report() {
  const theme = useTheme({ syncNative: true }); // syncNative＝标题栏颜色随应用主题（07-UX 2.4）
  useAgentColors(); // 注入设置页自定义的 Agent 身份色（按 Agent 维度条配色跟随）
  // 筛选状态：范围档 + 四维度（null=全部；2026-09-29 审查修复 #9b 起多选——
  // 数组=选中集合，可同时勾选多个 Agent/项目/模型/供应商做对比；项目空串成员=
  // 未知项目；供应商含 "unknown"=缺失组）。
  // 默认近 7 天（2026-09-24 所有者拍板：开窗即看本周节奏，比 30 天更常查）
  const [range, setRange] = useState("7d");
  const [agents, setAgents] = useState<string[] | null>(null);
  const [projects, setProjects] = useState<string[] | null>(null);
  const [models, setModels] = useState<string[] | null>(null);
  const [providers, setProviders] = useState<string[] | null>(null);
  // 数据：整页快照（会话明细已迁往会话窗口，M1-10）
  const [snap, setSnap] = useState<ReportSnapshot | null>(null);
  const [loaded, setLoaded] = useState(false);
  // 刷新失败信息（五轮审查）：额度页 loadError 同款纪律——查询失败不再
  // setSnap(null) 冒充「空态＋去设置引导」（把用户引向完全错误的排障方向）；
  // 有旧数据时顶部错误横幅＋保留旧数据，无旧数据时整页错误态＋重试
  const [loadError, setLoadError] = useState<string | null>(null);
  // 变更中轻量指示（五轮审查）：筛选/范围后续变更保留旧内容只降透明度，
  // 不再整页塌成「加载中…」重挂 ECharts（闪烁＋图例/交互状态丢失）
  const [fetching, setFetching] = useState(false);
  // 图表指标切换（数据已多指标下发，切换免回查）；维度条 token/次数/成本三档
  const [trendMetric, setTrendMetric] = useState<MetricKey>("token");
  // 趋势堆叠维度（#9a）：token 指标下可切「四分类（默认）/按 Agent/按模型」——
  // 多 Agent 对比在趋势层的落点；次数/时长/成本是单柱指标，切换器随之禁用
  const [stackDim, setStackDim] = useState<"token" | "agent" | "model">("token");
  const [heatMetric, setHeatMetric] = useState<MetricKey>("token");
  const [dimMetric, setDimMetric] = useState<"token" | "calls" | "cost">("token");
  // 手动刷新计数器（触发重拉）与「数据截至」时间戳（与 会话窗口 同一诚实模式）
  const [refreshTick, setRefreshTick] = useState(0);
  const [stamp, setStamp] = useState<string | null>(null);

  // 厂商注册表中文名（providerLabel 数据源，2026-09-29 审查修复 #10）：静态
  // 清单挂载拉一次，填充模块级映射后 bump 触发一次重渲染（维度条/曲线跟随）；
  // 2026-10-03 补取品牌色（额度曲线身份色用）
  const [kindsTick, setKindsTick] = useState(0);
  useEffect(() => {
    invoke<{ id: string; name: string; color: string }[]>("provider_kinds")
      .then((ks) => {
        kindNames = new Map(ks.map((k) => [k.id, k.name]));
        kindColors = new Map(ks.map((k) => [k.id, k.color]));
        setKindsTick((t) => t + 1);
      })
      .catch(() => {});
  }, []);

  // 导出反馈 toast（2026-10-09 toast 化）：成功带「打开所在目录」动作 6s 自动
  // 消失（悬停暂停），失败常驻可关闭——与设置页保存反馈同一范式（shared/Toast）；
  // 旧内联 tip 芯片会挤压头部按钮（文件名一长把范围档挤换行），连根移除
  const [toast, setToast] = useState<ToastData | null>(null);
  // 导出进行中（五轮审查防重入）：保存对话框挂起时再点「导出 CSV」会叠开第二个
  // 对话框/并发两次导出——进行中置位，完成（含用户取消）才复位
  const [exporting, setExporting] = useState(false);

  // 消费预算条（#23）：日/月预算线（设置页配置，0=不设）＋今日/本月估算成本；
  // 任一预算 >0 时在汇总卡后显示，超线红（对标 QuotaBar/LiteLLM spend budget）
  const [budget, setBudget] = useState<{
    daily: number;
    monthly: number;
    todayUsd: number | null;
    monthUsd: number | null;
  } | null>(null);
  // Agent 监控是否被显式清空（07-UX 1.6）：agents_enabled 键缺失＝缺省全启用，
  // 空数组才是用户显式清空——空态文案据此分岔，避免「默认配置下引导去勾选」
  // 的误导（新用户去设置页一看全勾着，一头雾水；真实原因是还没产生任何调用）
  const [agentsEmptied, setAgentsEmptied] = useState(false);
  useEffect(() => {
    let alive = true;
    (async () => {
      try {
        const [s, u] = await Promise.all([
          invoke<Record<string, string>>("get_settings"),
          invoke<[number | null, number | null] | null>("budget_usage"),
        ]);
        if (!alive) return;
        // Agent 展示序（2026-10-03 拖拽排序）：搭预算设置拉取的便车注入共享
        // 注册表，趋势图例「按 Agent」维度按用户序排列（Agent 下拉已由后端
        // report_options 按同序重排，前端零改动）
        setAgentOrder(parseAgentOrder(s.agents_order));
        // 空态分岔依据（07-UX 1.6）：仅显式空数组判「已清空」；键缺失/解析
        // 失败按缺省全启用处理（不误导）
        try {
          const list = s.agents_enabled !== undefined ? (JSON.parse(s.agents_enabled) as unknown) : null;
          setAgentsEmptied(Array.isArray(list) && list.length === 0);
        } catch {
          setAgentsEmptied(false);
        }
        const daily = Number(s.budget_daily_usd) || 0;
        const monthly = Number(s.budget_monthly_usd) || 0;
        if (daily > 0 || monthly > 0) {
          setBudget({
            daily,
            monthly,
            todayUsd: u?.[0] ?? null,
            monthUsd: u?.[1] ?? null,
          });
        }
      } catch {
        /* 预算数据拉取失败：不显示预算条（非核心路径）；
           设置读取失败按「已配置」处理（07-UX 1.6：保持新文案不误导） */
        if (alive) setAgentsEmptied(false);
      }
    })();
    return () => {
      alive = false;
    };
  }, [refreshTick]);
  const doExport = async () => {
    if (exporting) return; // 防重入（五轮审查）
    setExporting(true);
    try {
      const d = new Date();
      const p2 = (n: number) => String(n).padStart(2, "0");
      const stampText = `${d.getFullYear()}${p2(d.getMonth() + 1)}${p2(d.getDate())}-${p2(d.getHours())}${p2(d.getMinutes())}${p2(d.getSeconds())}`;
      const target = await save({
        title: "导出报表 CSV",
        defaultPath: `去你的岛-报表-${range}-${stampText}.csv`,
        filters: [{ name: "CSV 文件", extensions: ["csv"] }],
      });
      if (!target) return; // 用户取消
      try {
        const p = await invoke<string>("export_report_csv", {
          range,
          agent: agents,
          project: projects,
          model: models,
          provider: providers,
          path: target,
        });
        // 成功 toast：完整文件名放得下（浮层不占头部布局）；「打开所在目录」
        // 为显式点击（不自动弹资源管理器——四轮审查拍板保留），点击后 toast 收起
        setToast({
          text: `已导出：${p.split(/[\\/]/).pop()}`,
          kind: "ok",
          action: {
            label: "打开所在目录",
            onClick: () =>
              invoke("open_file_location", { path: p }).catch((e) =>
                setToast({ text: `打开目录失败：${e}`, kind: "error" }),
              ),
          },
          durationMs: 6000,
          pauseOnHover: true,
        });
      } catch (e) {
        // 失败常驻＋可关闭（共享 Toast 语义），全文换行可读不再截断
        setToast({ text: `导出失败：${e}`, kind: "error" });
      }
    } finally {
      setExporting(false);
    }
  };

  // 筛选/刷新变更：整页快照重拉（本地毫秒级）。
  // 首载与后续变更分轨（五轮审查）：首载（无旧数据）显示整页加载态；后续变更
  // 保留旧内容只降透明度（fetching），数据到达原位 setOption——ECharts 实例
  // 不重建，用户手动隐藏的图例系列/坐标缩放等交互状态得以保留
  useEffect(() => {
    let alive = true;
    if (snap == null) setLoaded(false);
    setFetching(true);
    setLoadError(null);
    (async () => {
      try {
        const s = await invoke<ReportSnapshot>("report_snapshot", {
          range,
          agent: agents,
          project: projects,
          model: models,
          provider: providers,
        });
        if (!alive) return;
        setSnap(s);
        setStamp(fmtDTFull(Date.now()));
      } catch (e) {
        if (!alive) return;
        // 保留旧数据（原先 setSnap(null) 把查询失败伪装成空态，见 loadError 注释）
        setLoadError(String(e));
      } finally {
        if (alive) {
          setLoaded(true);
          setFetching(false);
        }
      }
    })();
    return () => {
      alive = false;
    };
  }, [range, agents, projects, models, providers, refreshTick]);

  // 图表主题色（M1-4）：轴线/图例文字、网格线随主题切换；
  // 序列色与热力色阶同样随主题分档（四轮审查实算修正：原「高饱和双主题通用」
  // 假设不成立——深色原值在浅底 #f5f6f8 上 1.5~2.5:1，低于 WCAG 图形 3:1）
  const axisText = { color: theme === "dark" ? "#9ca3af" : "#57606a" };
  // splitLine 深色 0.06→0.09（五轮审查）：0.06 在深底上近不可见，参考线
  // 失去「对照刻度」的作用；仍属装饰性参考线（WCAG 豁免），不追 3:1
  const splitLine = theme === "dark" ? "rgba(255,255,255,0.09)" : "rgba(0,0,0,0.08)";
  // scroll 图例翻页控件配色随主题（2026-10-03 布局修复）：趋势图与额度曲线
  // 图例改 type:"scroll" 单行滚动（多项时不再折行压住绘图区），ECharts 翻页
  // 箭头默认深蓝、翻页数字默认黑，双主题下都不可读——两态箭头＋数字一并覆盖
  const legendScroll =
    theme === "dark"
      ? {
          pageIconColor: "#9ca3af",
          pageIconInactiveColor: "rgba(156,163,175,0.35)",
          pageTextStyle: { color: "#9ca3af" },
        }
      : {
          pageIconColor: "#57606a",
          pageIconInactiveColor: "rgba(87,96,106,0.35)",
          pageTextStyle: { color: "#57606a" },
        };
  // 序列色双档：浅色用深一档变体（浅底实算全部 ≥3.4:1：输入 #2563eb=4.76、
  // 输出 #0d9488=3.46、缓存读 #7c3aed=5.27、缓存写 #b45309=5.1、时长 #0891b2=3.41、
  // 次数 #0284c7=3.79、成本 #047857=5.07）；深色侧原值不动（已验收观感）
  const seriesColors =
    theme === "dark"
      ? {
          duration: "#22d3ee",
          calls: "#38bdf8",
          cost: "#10b981",
          input: "#60a5fa",
          output: "#2dd4bf",
          cache_read: "#a78bfa",
          cache_creation: "#fbbf24",
        }
      : {
          duration: "#0891b2",
          calls: "#0284c7",
          cost: "#047857",
          input: "#2563eb",
          output: "#0d9488",
          cache_read: "#7c3aed",
          cache_creation: "#b45309",
        };
  // 气泡随主题（2026-10-03 审查修复）：ECharts 默认白底黑字，深色页面悬浮弹出
  // 亮白气泡——与自绘 Tip 组件的深色玻璃两套气泡语言；配方与 App.css .tip 同源
  const tipTheme =
    theme === "dark"
      ? {
          backgroundColor: "rgba(17,19,24,0.96)",
          borderColor: "rgba(255,255,255,0.12)",
          textStyle: { color: "#d1d5db", fontSize: 11 },
        }
      : {
          backgroundColor: "rgba(255,255,255,0.98)",
          borderColor: "rgba(0,0,0,0.12)",
          textStyle: { color: "#424a53", fontSize: 11 },
        };

  // 趋势图：粒度随范围自动（Rust 侧分桶）；今日档补齐 24 小时（空小时归零，曲线完整）
  const trendOption = useMemo<EChartsCoreOption>(() => {
    const t = snap?.trend ?? [];
    const byBucket = new Map(t.map((r) => [r.bucket, r]));
    let keys: string[];
    if (range === "today") {
      const d = new Date();
      const p = (n: number) => String(n).padStart(2, "0");
      const day = `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
      keys = Array.from({ length: 24 }, (_, i) => `${day} ${p(i)}:00`);
    } else {
      keys = t.map((r) => r.bucket);
    }
    const labels = keys.map((k) => bucketLabel(k, range));
    // grid 分支化（2026-10-03 布局修复）：top 按「有无图例」分档——原四分支
    // 共用 top:32 为图例预留，单柱模式无图例也预留，顶部 32px 空带使绘图区
    // 内容整体下沉；有图例档 = 图例 top 6 + 单行约 18 + 间隙
    const gridBase = { left: 56, right: 16, bottom: 24 };
    const gridWithLegend = { ...gridBase, top: 36 };
    const gridPlain = { ...gridBase, top: 12 };
    const xAxis = { type: "category" as const, data: labels, axisLabel: axisText };
    const mkYAxis = (fmt: (v: number) => string) => ({
      type: "value" as const,
      axisLabel: { ...axisText, formatter: fmt },
      splitLine: { lineStyle: { color: splitLine } },
    });
    // 时长模式：单柱（毫秒），y 轴短格式；无时长数据的桶计 0。
    // 青色系（seriesColors.duration）：避免与堆叠图「缓存写」黄同色不同义（2026-09-24 C1）
    if (trendMetric === "duration") {
      return {
        tooltip: {
          ...tipTheme,
          trigger: "axis",
          valueFormatter: (v: number) => fmtDuration(v),
        },
        // 单柱模式显式关图例（2026-10-03 布局修复）：原分支缺 legend 字段，
        // Chart 走 merge setOption，组件级 merge 语义下上一次 token 模式的
        // 图例整体残留显示且点击无效（对应系列已不存在）
        legend: { show: false, data: [] },
        grid: gridPlain,
        xAxis,
        yAxis: mkYAxis(fmtAxisDur),
        series: [
          {
            name: "生成时长",
            type: "bar",
            data: keys.map((k) => byBucket.get(k)?.duration_ms ?? 0),
            itemStyle: { color: seriesColors.duration },
            barMaxWidth: 26,
          },
        ],
      };
    }
    if (trendMetric === "calls") {
      return {
        tooltip: { ...tipTheme, trigger: "axis" },
        legend: { show: false, data: [] },
        grid: gridPlain,
        xAxis,
        yAxis: mkYAxis((v) => fmtTokens(v)),
        series: [
          {
            name: "调用次数",
            type: "bar",
            data: keys.map((k) => byBucket.get(k)?.calls ?? 0),
            itemStyle: { color: seriesColors.calls },
            barMaxWidth: 26,
          },
        ],
      };
    }
    // 成本模式（M3-1）：单柱美元，emerald 绿（钱=绿直觉；单柱模式与其余序列互斥不撞色）
    if (trendMetric === "cost") {
      return {
        tooltip: {
          ...tipTheme,
          trigger: "axis",
          valueFormatter: (v: number) => fmtUsd(v),
        },
        legend: { show: false, data: [] },
        grid: gridPlain,
        xAxis,
        yAxis: mkYAxis(fmtAxisUsd),
        series: [
          {
            name: "估算成本",
            type: "bar",
            data: keys.map((k) => byBucket.get(k)?.cost ?? 0),
            itemStyle: { color: seriesColors.cost },
            barMaxWidth: 26,
          },
        ],
      };
    }
    // token 模式：堆叠四项 + tooltip 补思考与合计行（合计=账单口径四项；思考单列不混入）
    // #9a 堆叠维度切换：非「四分类」时按 Agent/模型分系列堆叠（多 Agent 对比）
    if (trendMetric === "token" && stackDim !== "token") {
      const slices =
        stackDim === "agent" ? (snap?.trend_agent_slices ?? []) : (snap?.trend_model_slices ?? []);
      // 维度值集合（保序去重）＋(桶,维度值)→token 索引；
      // Agent 维度按用户拖拽序排列（图例与堆叠顺序一致；模型维度保持
      // 数据出现序——模型清单与 Agent 序无关）
      const deduped: string[] = [];
      for (const sl of slices) {
        if (!deduped.includes(sl.label)) deduped.push(sl.label);
      }
      const labels = stackDim === "agent" ? orderAgentIds(deduped) : deduped;
      const idx = new Map<string, number>();
      for (const sl of slices) {
        idx.set(`${sl.bucket}\u{0}${sl.label}`, sl.total);
      }
      const colorOf = (l: string) =>
        stackDim === "agent" ? agentColor(l) : hashColor(l, theme);
      // 图例＝紧凑位（2026-09-30 名称口径统一）：Agent 维度走短名，替换旧裸 id
      const nameOf = (l: string) =>
        stackDim === "agent"
          ? l
            ? agentShortName(l)
            : "未知"
          : l === ""
            ? "未知模型"
            : l;
      return {
        tooltip: {
          ...tipTheme,
          trigger: "axis",
          formatter: (params: unknown) => {
            const arr = (Array.isArray(params) ? params : [params]) as {
              marker: string;
              seriesName: string;
              value: number;
              axisValue: string;
            }[];
            if (arr.length === 0) return "";
            const lines = arr
              .filter((p) => p.value > 0)
              .map((p) => `${p.marker}${p.seriesName}　${fmtTokens(p.value)}`);
            const sum = arr.reduce((acc, p) => acc + (p.value || 0), 0);
            return `${arr[0].axisValue}<br/>${lines.join("<br/>")}<br/>合计　${fmtTokens(sum)}`;
          },
        },
        // 显式 legend（2026-10-03 布局修复）：top 0→6 与头部按钮拉开间距、
        // type:"scroll" 多维度值时单行滚动不折行（原折行第二行压进绘图区）
        legend: {
          show: true,
          type: "scroll",
          data: labels.map(nameOf),
          textStyle: axisText,
          top: 6,
          ...legendScroll,
        },
        grid: gridWithLegend,
        xAxis,
        yAxis: mkYAxis((v) => fmtTokens(v)),
        series: labels.map((l) => ({
          name: nameOf(l),
          type: "bar",
          stack: "total",
          data: keys.map((k) => idx.get(`${k}\u{0}${l}`) ?? 0),
          itemStyle: { color: colorOf(l) },
          barMaxWidth: 26,
        })),
      };
    }
    // 四分类堆叠（默认）
    return {
      tooltip: {
        ...tipTheme,
        trigger: "axis",
        // ECharts axis 触发的 formatter 参数是数组；此处只读 dataIndex/seriesName/value
        // eslint 不在项目内，any 限定在回调局部（option 类型对 formatter 参数宽泛）
        formatter: (params: unknown) => {
          const arr = (Array.isArray(params) ? params : [params]) as {
            dataIndex: number;
            seriesName: string;
            marker: string;
            value: number;
            axisValue: string;
          }[];
          if (arr.length === 0) return "";
          const lines = arr.map(
            (p) => `${p.marker}${p.seriesName}　${fmtTokens(p.value)}`,
          );
          // 合计按全量数据（四轮审查）：params 只含未被图例隐藏的系列，逐项
          // 相加在隐藏「缓存读」等系列后会把「合计」误读为全量——改从该桶
          // 原始行四项求和，与数据口径恒一致
          const row0 = byBucket.get(keys[arr[0].dataIndex]);
          const sum = row0
            ? row0.input + row0.output + row0.cache_read + row0.cache_creation
            : arr.reduce((acc, p) => acc + (p.value || 0), 0);
          let html = `${arr[0].axisValue}<br/>${lines.join("<br/>")}<br/>合计　${fmtTokens(sum)}`;
          if (row0 && row0.reasoning > 0) html += `<br/>思考　${fmtTokens(row0.reasoning)}`;
          return html;
        },
      },
      legend: {
        show: true,
        type: "scroll",
        data: STACK.map((s) => s.name),
        textStyle: axisText,
        top: 6,
        ...legendScroll,
      },
      grid: gridWithLegend,
      xAxis,
      yAxis: mkYAxis((v) => fmtTokens(v)),
      series: STACK.map((s) => ({
        name: s.name,
        type: "bar",
        stack: "total",
        data: keys.map((k) => byBucket.get(k)?.[s.key] ?? 0),
        itemStyle: { color: seriesColors[s.key] },
        barMaxWidth: 26,
      })),
    };
  }, [snap, range, trendMetric, stackDim, theme]);

  // 额度消耗历史：按 供应商×实例×窗口 分组成多条百分比曲线（5h 蓝锯齿／周窗口紫爬升）；
  // 2026-09-29 审查修复 #8：原按 供应商+窗口 分组，同厂商多实例（多账号）的采样
  // 点混进一条线互相穿插跳变；account 维度由后端 QuotaPoint 携带，实例名走
  // options.quota_accounts（厂商中文名·用户别名），存量无实例快照归"默认"线。
  // x 轴时间值轴，y 轴固定 0–100%；Rust 侧已按序列分组抽稀，前端直接画
  const quotaOption = useMemo<EChartsCoreOption>(() => {
    const pts = snap?.quota_curve ?? [];
    const accounts = snap?.options.quota_accounts ?? [];
    const groups = new Map<string, QuotaPoint[]>();
    for (const p of pts) {
      const k = `${p.provider}:${p.account_id ?? ""}:${p.window_kind}`;
      const list = groups.get(k);
      if (list) list.push(p);
      else groups.set(k, [p]);
    }
    return {
      tooltip: {
        ...tipTheme,
        trigger: "axis",
        formatter: (params: unknown) => {
          const arr = (Array.isArray(params) ? params : [params]) as {
            seriesName: string;
            marker: string;
            value: [number, number];
          }[];
          if (arr.length === 0) return "";
          const lines = arr.map(
            (p) => `${p.marker}${p.seriesName}　${p.value[1].toFixed(1)}%`,
          );
          return `${fmtDTFull(arr[0].value[0])}<br/>${lines.join("<br/>")}`;
        },
      },
      // type:"scroll"（2026-10-03 布局修复）：多实例×多窗口曲线多时图例不再
      // 折行压住绘图区；top 0→6 与卡片头部拉开间距，grid top 配套 30→36
      legend: { type: "scroll", textStyle: axisText, top: 6, ...legendScroll },
      grid: { left: 44, right: 16, top: 36, bottom: 24 },
      xAxis: { type: "time", axisLabel: axisText },
      yAxis: {
        min: 0,
        max: 100,
        axisLabel: { ...axisText, formatter: "{value}%" },
        splitLine: { lineStyle: { color: splitLine } },
      },
      series: [...groups.entries()].map(([key, list]) => {
        const kind = list[0].window_kind;
        const acc = list[0].account_id
          ? accounts.find((a) => a.account_id === list[0].account_id)
          : undefined;
        const base = acc
          ? acc.alias
            ? accountLabel(acc.kind_name, acc.alias)
            : acc.kind_name
          : providerLabel(list[0].provider);
        // 身份色（2026-10-03 审查修复）：按厂商品牌色取色（注册表投影），
        // 窗口类型用实线/虚线区分（5h 实线、周窗口虚线）——原固定色
        // 5h 蓝＝Codex 身份色、weekly 紫＝Qwen 身份色，曲线会被误读为某 Agent
        const identity =
          kindColors.get(acc?.provider ?? list[0].provider) ?? hashColor(key, theme);
        return {
          name: `${base} ${windowLabel(kind)}`,
          type: "line",
          showSymbol: false,
          data: list.map((p) => [p.fetched_at, p.used_percent]),
          itemStyle: { color: identity },
          lineStyle: { width: 1.5, type: kind === "weekly" ? "dashed" : "solid" },
        };
      }),
    };
  }, [snap, theme, kindsTick]);

  // 热力图：列=小时，行=星期，色阶随指标切换。
  // max 取 P95 封顶（2026-09-24 C3）：单次爆发不淹没日常格子，
  // 超 P95 的格子统一最深色（outOfRange），悬浮仍显示真实值
  const heatOption = useMemo<EChartsCoreOption>(() => {
    const heat = snap?.heatmap ?? [];
    const val = (c: HeatCell) => (heatMetric === "calls" ? c.calls : c.total);
    const vals = heat.map(val).sort((a, b) => a - b);
    const p95 = vals.length > 0 ? vals[Math.min(vals.length - 1, Math.floor(vals.length * 0.95))] : 1;
    const max = Math.max(1, p95);
    return {
      tooltip: {
        ...tipTheme,
        formatter: (p: { data: [number, number, number] }) =>
          `${WEEKDAYS[p.data[1]]} ${p.data[0]}:00 · ${heatMetric === "calls" ? `${p.data[2]} 次` : fmtTokens(p.data[2])}`,
      },
      grid: { left: 44, right: 20, top: 10, bottom: 52 },
      xAxis: {
        type: "category",
        data: Array.from({ length: 24 }, (_, i) => `${i}`),
        axisLabel: axisText,
        splitArea: { show: true },
      },
      yAxis: { type: "category", data: WEEKDAYS, axisLabel: axisText },
      visualMap: {
        min: 0,
        max,
        calculable: true,
        orient: "horizontal",
        left: "center",
        bottom: 0,
        textStyle: axisText,
        inRange: { color: theme === "dark" ? ["#4a6a9b", "#38bdf8", "#34d399"] : ["#1e3a5f", "#38bdf8", "#34d399"] },
        outOfRange: { color: "#34d399" },
      },
      series: [
        {
          type: "heatmap",
          data: heat.map((c) => [c.hour, c.weekday, val(c)]),
        },
      ],
    };
  }, [snap, heatMetric, theme]);

  const summary = snap?.summary;
  const prev = snap?.prev_summary ?? null;
  const empty = loaded && (snap == null || (summary != null && summary.calls === 0));
  const hasFilter = agents != null || projects != null || models != null || providers != null;
  // 维度条点击＝toggle 进/出对应筛选集合（#9b 多选对比；清到空集合归 null=全部）
  const toggleOf =
    (setter: React.Dispatch<React.SetStateAction<string[] | null>>) => (label: string) =>
      setter((cur) =>
        cur == null
          ? [label]
          : cur.includes(label)
            ? cur.length === 1
              ? null
              : cur.filter((x) => x !== label)
            : [...cur, label],
      );

  /** 会话数卡跳转会话窗口（跨窗口动线：报表看总量，明细去会话） */
  const openSessions = () => {
    invoke("show_sessions_window").catch(() => {});
  };

  return (
    <div className={`rp-root${fetching ? " fetching" : ""}`}>
      {/* 导出反馈 toast（fixed 贴窗顶居中，不占头部布局——挤压问题根治于此） */}
      <Toast data={toast} onDismiss={() => setToast(null)} />
      <div className="rp-header">
        {/* 页内标题与全项目入口命名对齐（窗口标题/托盘菜单/面板链接均叫「报表」，
            「用量」语义由页内汇总卡承载） */}
        <span className="rp-title">报表</span>
        <div className="rp-header-right">
          {stamp && (
            <Tip content="打开/刷新时查询本地数据库的时间点（不做自动轮询）">
              <span className="rp-stamp">数据截至 {stamp.slice(5)}</span>
            </Tip>
          )}
          <button className="rp-btn" onClick={() => setRefreshTick((t) => t + 1)}>
            刷新
          </button>
          <button
            className="rp-btn"
            title="导出当前范围与筛选下的汇总卡＋维度条＋趋势明细 CSV（与页面口径一致）"
            onClick={doExport}
            disabled={exporting}
          >
            {/* 按钮文案与导出 CSV 语义明示（与会话页「导出 CSV」统一，明示格式降低理解成本） */}
            {exporting ? "导出中…" : "导出 CSV"}
          </button>
          {/* 会话级导出已随会话明细迁往会话窗口（M1-10）；报表聚合导出见 doExport（#11） */}
          <div className="rp-ranges">
            {RANGES.map((r) => (
              <button
                key={r.key}
                className={`rp-btn${range === r.key ? " active" : ""}`}
                onClick={() => setRange(r.key)}
              >
                {r.label}
              </button>
            ))}
          </div>
          {/* 设置入口（07-UX 1.3）：报表页正常状态下无任何设置入口，「改个主题/
              阈值」须回忆托盘——头部右侧组末端补文字按钮（与额度页「设置」同款
              同文案；本项目按钮语言为「文字为体」，不做 icon-only 新形态族），
              不做跨窗口导航条 */}
          <button
            className="rp-btn"
            title="打开设置窗口"
            onClick={() => invoke("show_settings_window").catch(() => {})}
          >
            设置
          </button>
        </div>
      </div>

      {!loaded ? (
        // 首载加载态（2026-10-03 审查修复）：此前首载期间整页空白无指示。
        // 加载中与筛选空态（07-UX 2.1）保持轻量一行灰字，不上真空态模板
        <div className="rp-empty">加载中…</div>
      ) : loadError != null && snap == null ? (
        // 首载失败：显式错误态＋重试（五轮审查，对齐额度页纪律）——
        // 原先失败冒充空态并引导「去设置勾选」，排障方向完全错误；
        // 2.1 起统一走 EmptyState 模板（WarnIcon 与真空态区分排障方向）
        <EmptyState
          icon={<WarnIcon />}
          title="加载失败"
          desc={loadError}
          action={{ label: "重试", onClick: () => setRefreshTick((t) => t + 1) }}
        />
      ) : empty ? (
        hasFilter ? (
          // 筛选空态：用户任务中途，保持轻量文案（07-UX 2.1）
          <div className="rp-empty">
            当前筛选条件下暂无数据，可清除筛选或放宽时间范围
          </div>
        ) : agentsEmptied ? (
          // 仅显式清空全部勾选才引导去设置（07-UX 1.6）：默认配置下 agents_enabled
          // 非空，按旧文案去检查只会一头雾水——真实原因是还没有产生任何调用
          <EmptyState
            icon={<ReportIcon />}
            title="暂无数据"
            desc="请先在设置页勾选要监控的 Agent，数据会随使用自动积累"
            action={{
              label: "去设置勾选",
              onClick: () => invoke("show_settings_window").catch(() => {}),
            }}
          />
        ) : (
          // 真空态（07-UX 2.1 统一模板）：默认配置下无任何调用记录，
          // 无主动作可给（不造无意义按钮），交代数据积累预期即可
          <EmptyState
            icon={<ReportIcon />}
            title="还没有调用记录"
            desc="开始使用任意 Agent 后，数据会自动积累"
          />
        )
      ) : (
        snap && (
          <>
            {/* 刷新失败但手上有旧数据（五轮审查）：错误横幅＋保留旧内容＋重试，
                「数据截至」时间戳不更新即为旧数据口径的可见暗示 */}
            {loadError != null && (
              <div className="rp-error">
                <span>{`刷新失败：${loadError}——以下为截至 ${stamp ?? "上次成功查询"} 的数据`}</span>
                <button
                  type="button"
                  className="rp-btn"
                  onClick={() => setRefreshTick((t) => t + 1)}
                >
                  重试
                </button>
              </div>
            )}
            {/* 汇总卡行：第一眼看清总量、强度与效率（时长/首字为 null 显示 — 降级）；
                口径细节收进各卡 ⓘ（2026-09-24 C2），页脚只留总说明 */}
            {(() => {
              const s = snap.summary;
              // 思考占比：思考 /（输入+输出+思考），缓存读写不计入
              const denom = s.reasoning_tokens + s.billable_tokens;
              const ratio =
                denom > 0
                  ? (s.reasoning_tokens / denom) * 100 < 1
                    ? "<1%"
                    : `${Math.round((s.reasoning_tokens / denom) * 100)}%`
                  : "—";
              // 缓存命中率：缓存读 /（输入 + 缓存读）——命中越高重复上下文越省
              const hitDenom = s.input_total + s.cache_read_total;
              const hit = hitDenom > 0 ? Math.round((s.cache_read_total / hitDenom) * 100) : null;
              // 估算成本（M3-1）：缺价模型不计入，计数进 ⓘ 提示引导补覆盖
              // （语义分行：口径主行＋缺价条件行，缺价段只在存在时出现）
              const costTip = twoTip(
                "按当前单价估算（LiteLLM 单价快照＋用户覆盖表），价格变动不追溯",
                s.missing_price_count > 0
                  ? `${s.missing_price_count} 个模型缺少单价，未计入该部分（可在单价覆盖表补齐）`
                  : undefined,
              );
              const cards: {
                key: string;
                label: string;
                val: string;
                /** 数值悬浮兜底（C1）：窄窗 9 列下长值会 ellipsis 截断，title 给
                 *  「标签＋完整值」；缺省由渲染处按 label＋val 拼装 */
                valTip?: string;
                tip?: React.ReactNode;
                delta?: React.ReactNode;
                warn?: boolean;
                link?: boolean;
                onClick?: () => void;
              }[] = [
                {
                  key: "tokens",
                  label: "总 Token",
                  val: fmtTokens(s.total_tokens),
                  // 屏显为 K/M 缩写，title 给精确千分位（真实增量信息）
                  valTip: `总 Token：${s.total_tokens.toLocaleString()}`,
                  tip: twoTip(
                    "统计口径：输入＋输出＋缓存读写的全量 token",
                    "与官方账单同口径",
                  ),
                  delta: <Delta value={s.total_tokens} prev={prev?.total_tokens ?? null} />,
                },
                {
                  key: "cost",
                  label: "估算成本",
                  val: s.est_cost != null ? fmtUsd(s.est_cost) : "—",
                  tip: costTip,
                  delta: <Delta value={s.est_cost} prev={prev?.est_cost ?? null} />,
                },
                {
                  key: "calls",
                  label: "调用次数",
                  val: s.calls.toLocaleString(),
                  delta: <Delta value={s.calls} prev={prev?.calls ?? null} />,
                },
                {
                  key: "duration",
                  label: "生成时长",
                  val: fmtDuration(s.duration_ms),
                  tip: twoTip(
                    "模型生成时长合计",
                    "仅 ZCode 等转录含时长字段的 Agent 有数据，无则显示 —",
                  ),
                  delta: <Delta value={s.duration_ms} prev={prev?.duration_ms ?? null} />,
                },
                {
                  key: "ttft",
                  label: "平均首字",
                  val: s.ttft_avg_ms != null ? fmtMs(s.ttft_avg_ms) : "—",
                  tip: twoTip(
                    "首字延迟（TTFT）按调用平均",
                    "仅含上报该字段的调用；分模型对比见下方维度区",
                  ),
                  delta: <Delta value={s.ttft_avg_ms} prev={prev?.ttft_avg_ms ?? null} />,
                },
                {
                  key: "reason",
                  label: "思考占比",
                  val: ratio,
                  tip: twoTip(
                    "思考占比＝思考／（输入＋输出＋思考）",
                    "缓存读写不计入；极小占比显示 <1%",
                  ),
                },
                {
                  key: "cache",
                  label: "缓存命中率",
                  val: hit != null ? `${hit}%` : "—",
                  tip: twoTip(
                    "缓存命中率＝缓存读／（输入＋缓存读）",
                    "命中越高，重复上下文的生成越省时省额度",
                  ),
                },
                {
                  key: "sessions",
                  label: "会话数",
                  val: s.sessions.toLocaleString(),
                  delta: <Delta value={s.sessions} prev={prev?.sessions ?? null} />,
                  link: true,
                  onClick: openSessions,
                },
                {
                  key: "errors",
                  label: "出错次数",
                  val: s.errors.toLocaleString(),
                  warn: s.errors > 0,
                  delta: <Delta value={s.errors} prev={prev?.errors ?? null} warnUp />,
                },
              ];
              return (
                <div className="rp-cards">
                  {cards.map((c) => (
                    <div
                      key={c.key}
                      className={`rp-summ${c.warn ? " warn" : ""}${c.link ? " link" : ""}`}
                      title={c.onClick ? "点击打开会话窗口" : undefined}
                      /* 跳转卡键盘可达（2026-10-03 审查补充）：会话数卡 Enter 打开会话窗口 */
                      {...(c.onClick
                        ? {
                            role: "button",
                            tabIndex: 0,
                            onClick: c.onClick,
                            onKeyDown: (e: React.KeyboardEvent) => {
                              // ⓘ（Tip 注入的聚焦件）聚焦时放行其自身激活，
                              // 不冒泡执行卡片跳转（五轮审查，同设置页 st-agent 卡）；
                              // Enter＋Space 双键（2026-10-10 审查 a11y 对齐）
                              if (e.target !== e.currentTarget) return;
                              if (e.key === "Enter" || e.key === " ") {
                                e.preventDefault();
                                c.onClick?.();
                              }
                            },
                          }
                        : {})}
                    >
                      <div className="rp-summ-label">
                        {c.label}
                        {c.tip && (
                          <Tip content={c.tip}>
                            <span className="rp-info">
                              <InfoIcon />
                            </span>
                          </Tip>
                        )}
                      </div>
                      {/* 数值悬浮兜底（C1）：960~1100px 窗口 9 列约 88px/张，长值
                          ellipsis 截断后无提示即不可读——原生 title 给完整值
                          （与维度条 .rp-dim-name 先例一致，纯兜底不进 Tab 序） */}
                      <div className="rp-summ-val" title={c.valTip ?? `${c.label}：${c.val}`}>
                        {c.val}
                      </div>
                      {c.delta}
                    </div>
                  ))}
                </div>
              );
            })()}

            {/* 消费预算条（#23）：设置页配置了任一预算线时显示；超线红色 */}
            {budget && (
              <div className="rp-budget">
                {budget.daily > 0 && (
                  <span
                    className={
                      (budget.todayUsd ?? 0) >= budget.daily ? "rp-budget-item over" : "rp-budget-item"
                    }
                  >
                    {/* null＝无可计价数据，与同屏「估算成本 —」同口径（五轮审查：
                        原先折算 $0.00 会被读作「今天一分没花」——同一屏两种答案） */}
                    今日 <b>{budget.todayUsd != null ? fmtUsd(budget.todayUsd) : "—"}</b> / 预算{" "}
                    {fmtUsd(budget.daily)}
                  </span>
                )}
                {budget.monthly > 0 && (
                  <span
                    className={
                      (budget.monthUsd ?? 0) >= budget.monthly ? "rp-budget-item over" : "rp-budget-item"
                    }
                  >
                    本月 <b>{budget.monthUsd != null ? fmtUsd(budget.monthUsd) : "—"}</b> / 预算{" "}
                    {fmtUsd(budget.monthly)}
                  </span>
                )}
                <Tip
                  content={twoTip(
                    "与「估算成本」同口径：当前单价 × 历史用量（USD，价格变动不追溯）",
                    "订阅制额度不折算；预算线在设置页「额度与凭据」配置",
                  )}
                >
                  <span className="rp-budget-tip">
                    <InfoIcon />
                  </span>
                </Tip>
              </div>
            )}

            {/* 筛选条：可搜索下拉（与维度条点击双向联动）；#9b 起多选——
                可同时勾选多个值做对比（选「全部」清除该维度） */}
            <div className="rp-filter">
              <SearchSelect
                multi
                values={agents}
                allLabel="全部 Agent"
                onValues={setAgents}
                options={snap.options.agents.map((a) => ({ value: a, label: agentFullName(a) }))}
              />
              <SearchSelect
                multi
                values={projects}
                allLabel="全部项目"
                onValues={setProjects}
                options={snap.options.projects.map((p) => ({
                  value: p,
                  label: tail(p) || "未知项目",
                }))}
              />
              <SearchSelect
                multi
                values={models}
                allLabel="全部模型"
                onValues={setModels}
                options={snap.options.models.map((m) => ({ value: m, label: m }))}
              />
              <SearchSelect
                multi
                values={providers}
                allLabel="全部供应商"
                onValues={setProviders}
                options={snap.options.providers.map((p) => ({
                  value: p,
                  label: p === "unknown" ? "未知供应商" : providerLabel(p),
                }))}
              />
              {hasFilter && (
                <button
                  className="rp-btn rp-clear"
                  onClick={() => {
                    setAgents(null);
                    setProjects(null);
                    setModels(null);
                    setProviders(null);
                  }}
                >
                  清除筛选
                </button>
              )}
            </div>

            {/* 主区：维度面板在前（宽窗居左，点击条目联动全局筛选）+ 趋势在后（宽窗居右）——
                2026-10-08 所有者拍板换位：维度条是全局筛选的发起方，
                「控件在前、结果在后」的筛选动线；两卡宽度份额随之对调
                （维度 2fr、趋势 3fr），卡片内部内容零改动；窄窗单列按 DOM
                顺序堆叠，维度分布同样在前（首版曾在 960 断点用 order 把趋势
                提回首行，所有者复验拍板单列同步换位，勿再加回） */}
            <div className="rp-main">
              <section className="rp-card rp-dims">
                <div className="rp-dims-head">
                  <span className="rp-dims-title">维度分布</span>
                  <MetricToggle
                    value={dimMetric}
                    onChange={(v) =>
                      setDimMetric(v === "calls" ? "calls" : v === "cost" ? "cost" : "token")
                    }
                    options={[
                      { key: "token", label: "Token" },
                      { key: "calls", label: "次数" },
                      { key: "cost", label: "成本" },
                    ]}
                  />
                </div>
                <DimBars
                  title="按 Agent"
                  items={snap.by_agent}
                  active={agents}
                  onToggle={toggleOf(setAgents)}
                  colorFn={agentColor}
                  hashTheme={theme}
                  fmt={agentFullName}
                  metric={dimMetric}
                />
                <DimBars
                  title={`按项目（${snap.by_project.length}）`}
                  items={snap.by_project}
                  active={projects}
                  onToggle={toggleOf(setProjects)}
                  hashTheme={theme}
                  fmt={(l) => tail(l) || "未知项目"}
                  metric={dimMetric}
                />
                <DimBars
                  title="按模型"
                  items={snap.by_model}
                  active={models}
                  onToggle={toggleOf(setModels)}
                  hashTheme={theme}
                  metric={dimMetric}
                />
                {/* 按模型首字延迟：条长=平均首字（降序），点击同样联动模型筛选；
                    成本档下该段与错误分布无对应口径，随面板隐藏 */}
                {dimMetric !== "cost" && snap.ttft_by_model.length > 0 && (
                  <DimBars
                    title="按模型首字延迟"
                    items={snap.ttft_by_model}
                    active={models}
                    onToggle={toggleOf(setModels)}
                    hashTheme={theme}
                    fmtVal={(t) => fmtMs(t)}
                    tipFormat={(it, name) =>
                      `${name} · 平均首字 ${fmtMs(it.total)} · ${it.calls.toLocaleString()} 次有首字记录的调用（点击筛选该模型）`
                    }
                    sortable={false}
                    metric={dimMetric}
                  />
                )}
                <DimBars
                  title="按供应商"
                  items={snap.by_provider}
                  active={providers}
                  onToggle={toggleOf(setProviders)}
                  hashTheme={theme}
                  fmt={(l) => (l === "unknown" ? "未知供应商" : providerLabel(l))}
                  metric={dimMetric}
                />
                {/* 错误分布：label 是原始 error_type，中文归大类展示；条长=次数（红系语义色） */}
                {dimMetric !== "cost" && snap.by_error.length > 0 && (
                  <DimBars
                    title="按错误类型"
                    items={snap.by_error}
                    active={null}
                    onToggle={() => {}}
                    selectable={false}
                    hashTheme={theme}
                    fmt={errorReason}
                    colorFn={() => "var(--danger)"}
                    fmtVal={(t) => t.toLocaleString()}
                    tipFormat={(it, name) => `${name} · ${it.total.toLocaleString()} 次`}
                    metric={dimMetric}
                  />
                )}
              </section>
              <section className="rp-card rp-trend">
                <div className="rp-card-head">
                  <div className="rp-card-title">
                    用量趋势（
                    {range === "today"
                      ? "按小时"
                      : range === "90d"
                        ? "按周"
                        : range === "all"
                          ? "按月"
                          : "按日"}
                    ）
                  </div>
                  {/* 两组按钮包进右侧容器成组（2026-10-03 布局修复）：原头部
                      space-between 三元素散开（标题｜指标组｜维度组居中），
                      且维度组随指标条件卸载——切到次数/时长/成本时指标组跳到
                      最右缘、切回又跳回中间。容器化后头部恒为「标题｜右侧组」
                      两元素，按钮位置永不跳 */}
                  <div className="rp-head-toggles">
                    <MetricToggle
                      value={trendMetric}
                      onChange={setTrendMetric}
                      options={[
                        { key: "token", label: "Token" },
                        { key: "calls", label: "次数" },
                        { key: "duration", label: "时长" },
                        { key: "cost", label: "成本" },
                      ]}
                    />
                    {/* #9a 堆叠维度切换：仅 token 指标有意义（其余为单柱），随指标
                        禁用。2026-10-03 布局修复：原条件卸载与注释「随指标禁用」
                        不符且引发头部重排跳位——改恒渲染整组禁用占位 */}
                    <MetricToggle
                      value={stackDim}
                      onChange={(v) =>
                        setStackDim(v === "agent" ? "agent" : v === "model" ? "model" : "token")
                      }
                      options={[
                        { key: "token", label: "四分类" },
                        { key: "agent", label: "按 Agent" },
                        { key: "model", label: "按模型" },
                      ]}
                      disabled={trendMetric !== "token"}
                      disabledTip="仅 Token 指标支持分类对比"
                    />
                  </div>
                </div>
                <Chart
                  option={trendOption}
                  /* 双维自适应（五轮审查）＋高度同源（2026-10-03 布局修复）：
                     clamp 公式移至 .rp-main 的 --rp-trend-h 变量，维度卡
                     max-height 按同一变量 calc 钳制——两卡随视口同涨同落，
                     stretch 下底缘恒齐，矮视口不再出现图表下方大空白；
                     ResizeObserver 对容器两向变化都会触发 resize，行为不变 */
                  height="var(--rp-trend-h)"
                />
              </section>
            </div>

            {/* 年度热力图（2026-09-29 审查新增 #24）：GitHub 风格逐日用量格——
                周×小时看"一天内节奏"，这个看"全年坚持度/爆发日"；恒近 365 天
                全量（不随筛选联动，评审后暂缓），空数据整卡隐藏 */}
            <YearHeatmap refreshTick={refreshTick} />

            {/* 额度消耗历史（quota_snapshots 采样曲线；未配置额度整卡隐藏） */}
            {snap.quota_curve.length > 0 && (
              <section className="rp-card">
                <div className="rp-card-head">
                  <div className="rp-card-title">
                    额度消耗历史
                    <Tip
                      content={twoTip(
                        "按采集周期（约 5 分钟）采样的供应商额度用量百分比",
                        "5h 窗口呈锯齿（用满→重置），周窗口缓慢爬升；未配置额度的供应商不显示",
                      )}
                    >
                      <span className="rp-info">
                        <InfoIcon />
                      </span>
                    </Tip>
                  </div>
                </div>
                <Chart option={quotaOption} height="clamp(150px, min(20vh, 24vw), 260px)" />
              </section>
            )}

            {/* 热力图（本机时区） */}
            <section className="rp-card">
              <div className="rp-card-head">
                <div className="rp-card-title">周 × 小时分布（本机时区）</div>
                <MetricToggle
                  value={heatMetric}
                  onChange={(v) => setHeatMetric(v === "calls" ? "calls" : "token")}
                  options={[
                    { key: "token", label: "Token" },
                    { key: "calls", label: "次数" },
                  ]}
                />
              </div>
              <Chart option={heatOption} height="clamp(200px, min(30vh, 34vw), 380px)" />
            </section>

            {/* 会话明细已迁往独立会话窗口（M1-10）：本页收敛为纯聚合分析 */}

            {/* 页脚两行短句（07-UX 3.3）：原 60 余字单句塞四个口径难扫读，
                按语义拆「口径与成本」＋「明细指引」两行 */}
            <div className="rp-foot">
              <div>口径细节见各卡 ⓘ；成本按当前单价估算，不代表官方计费</div>
              <div>逐会话明细见「会话」窗口（岛面板或托盘菜单可开）</div>
            </div>
          </>
        )
      )}
    </div>
  );
}
