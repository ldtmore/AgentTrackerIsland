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
import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
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
import { fmtDTFull, fmtMs, tail } from "../shared/format";
import {
  fmtTokens,
  fmtDuration,
  agentColor,
  hashColor,
  errorReason,
  windowLabel,
} from "../shared/types";
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
}

/** 单维度分组行（token 总量 + 调用次数；首字延迟行 total=平均毫秒；错误分布里 total/calls 同为次数） */
interface SliceUsage {
  label: string;
  total: number;
  calls: number;
}

/** 趋势行（时间桶 + 四项用量 + 思考 + 次数 + 生成时长，指标切换免回查） */
interface TrendRow {
  bucket: string;
  input: number;
  output: number;
  cache_read: number;
  cache_creation: number;
  reasoning: number;
  calls: number;
  duration_ms: number | null;
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
  window_kind: string;
  fetched_at: number;
  used_percent: number;
}

/** 筛选下拉选项（projects 中空串=未知项目；providers 中 "unknown"=缺失供应商） */
interface FilterOptions {
  agents: string[];
  projects: string[];
  models: string[];
  providers: string[];
}

/** 整页快照 */
interface ReportSnapshot {
  summary: SummaryStats;
  /** 上一等长周期汇总（环比基准）；all 档为 null 不比 */
  prev_summary: SummaryStats | null;
  trend: TrendRow[];
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

/** 堆叠分类与配色（暗色科技风，与岛面板一致） */
const STACK = [
  { key: "input" as const, name: "输入", color: "#60a5fa" },
  { key: "output" as const, name: "输出", color: "#34d399" },
  { key: "cache_read" as const, name: "缓存读", color: "#a78bfa" },
  { key: "cache_creation" as const, name: "缓存写", color: "#fbbf24" },
];

/** 额度曲线配色：按窗口类型固定（5h 蓝／周窗口紫），与堆叠图语义不冲突 */
const QUOTA_COLORS: Record<string, string> = {
  "5h": "#38bdf8",
  weekly: "#a78bfa",
};

/** 趋势桶显示标签：今日→"HH:00"、7/30 天与 90 天→"MM-DD"、全部→"YYYY-MM" */
function bucketLabel(b: string, range: string): string {
  if (range === "today") return b.slice(11);
  if (range === "all") return b;
  return b.slice(5);
}

/** 可搜索下拉（筛选用）已抽至 shared/SearchSelect（M1-10：报表页与会话窗口共用） */

/** 指标键（趋势图三档；热力图与维度条两档无时长数据） */
type MetricKey = "token" | "calls" | "duration";

/** 毫秒 → 短格式 "45s"/"12m"/"1.3h"（趋势图 y 轴用，长格式会挤爆轴标签） */
function fmtAxisDur(ms: number): string {
  if (ms < 60_000) return `${Math.round(ms / 1000)}s`;
  if (ms < 3_600_000) return `${Math.round(ms / 60_000)}m`;
  return `${(ms / 3_600_000).toFixed(1)}h`;
}

/** 供应商显示名：glm → GLM（未知供应商口径值 "unknown" 由调用方映射） */
function providerLabel(p: string): string {
  return p.toUpperCase();
}

/** ⓘ 信息图标（与设置页同款）：标签尾「有更多说明」的功能性标记，悬停出 Tip 气泡 */
function InfoIcon() {
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
      <circle cx="12" cy="12" r="10" />
      <line x1="12" y1="16" x2="12" y2="12" />
      <line x1="12" y1="8" x2="12.01" y2="8" />
    </svg>
  );
}

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
      {up ? "▲" : "▼"} {Math.abs(Math.round(pct))}%
    </div>
  );
}

/** 指标切换小按钮组（Token/次数/时长，按图可配） */
function MetricToggle({
  value,
  onChange,
  options,
}: {
  value: MetricKey;
  onChange: (v: MetricKey) => void;
  options: { key: MetricKey; label: string }[];
}) {
  return (
    <div className="rp-metric">
      {options.map((o) => (
        <button
          key={o.key}
          className={`rp-metric-btn${value === o.key ? " on" : ""}`}
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
  onSelect,
  colorFn,
  fmt,
  fmtVal,
  tipFormat,
  selectable = true,
  sortable = true,
  metric = "token",
}: {
  title: string;
  items: SliceUsage[];
  /** 当前生效的筛选值（null=未筛选） */
  active: string | null;
  onSelect: (v: string | null) => void;
  /** 行颜色（缺省按名字散列稳定取色） */
  colorFn?: (label: string) => string;
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
  metric?: "token" | "calls";
}) {
  const [expanded, setExpanded] = useState(false);
  // 展示顺序：token 模式信任后端降序；calls 模式按次数重排（sortable=false 的
  // 特殊口径段——如首字延迟——固定后端顺序不重排）
  const ordered = useMemo(() => {
    if (!sortable || metric === "token") return items;
    return [...items].sort((a, b) => b.calls - a.calls);
  }, [items, sortable, metric]);
  const metricOf = (it: SliceUsage) => (metric === "calls" ? it.calls : it.total);
  const max = Math.max(1, ...ordered.map(metricOf));
  const shown = expanded ? ordered : ordered.slice(0, 8);
  const valText = (it: SliceUsage) =>
    metric === "calls" ? it.calls.toLocaleString() : fmtVal ? fmtVal(it.total) : fmtTokens(it.total);
  const tipOf = (it: SliceUsage, name: string, on: boolean) => {
    if (tipFormat) return tipFormat(it, name, on);
    const usage =
      metric === "calls"
        ? `${it.calls.toLocaleString()} 次调用 · ${fmtTokens(it.total)} token`
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
          const on = selectable && active === it.label;
          const name = fmt ? fmt(it.label) : it.label;
          return (
            <div
              key={it.label}
              className={`rp-dim-row${on ? " on" : ""}${selectable ? "" : " flat"}`}
              title={tipOf(it, name, on)}
              onClick={() => selectable && onSelect(on ? null : it.label)}
            >
              <span className="rp-dim-name" title={name}>
                {name}
              </span>
              <span className="rp-dim-track">
                <span
                  className="rp-dim-fill"
                  style={{
                    width: `${(metricOf(it) / max) * 100}%`,
                    background: colorFn ? colorFn(it.label) : hashColor(it.label),
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
          {expanded ? "收起 ▴" : `展开其余 ${ordered.length - 8} 项 ▾`}
        </button>
      )}
    </div>
  );
}

export default function Report() {
  const theme = useTheme();
  useAgentColors(); // 注入设置页自定义的 Agent 身份色（按 Agent 维度条配色跟随）
  // 筛选状态：范围档 + 四维度（null=全部；项目空串=未知项目；供应商 "unknown"=缺失）。
  // 默认近 7 天（2026-09-24 所有者拍板：开窗即看本周节奏，比 30 天更常查）
  const [range, setRange] = useState("7d");
  const [agent, setAgent] = useState<string | null>(null);
  const [project, setProject] = useState<string | null>(null);
  const [model, setModel] = useState<string | null>(null);
  const [provider, setProvider] = useState<string | null>(null);
  // 数据：整页快照（会话明细已迁往会话窗口，M1-10）
  const [snap, setSnap] = useState<ReportSnapshot | null>(null);
  const [loaded, setLoaded] = useState(false);
  // 图表指标切换（数据已多指标下发，切换免回查）；维度条仅 token/次数两档
  const [trendMetric, setTrendMetric] = useState<MetricKey>("token");
  const [heatMetric, setHeatMetric] = useState<MetricKey>("token");
  const [dimMetric, setDimMetric] = useState<"token" | "calls">("token");
  // 手动刷新计数器（触发重拉）与「数据截至」时间戳（与 会话窗口 同一诚实模式）
  const [refreshTick, setRefreshTick] = useState(0);
  const [stamp, setStamp] = useState<string | null>(null);

  // 筛选/刷新变更：整页快照重拉（本地毫秒级）
  useEffect(() => {
    let alive = true;
    setLoaded(false);
    (async () => {
      try {
        const s = await invoke<ReportSnapshot>("report_snapshot", {
          range,
          agent,
          project,
          model,
          provider,
        });
        if (!alive) return;
        setSnap(s);
        setStamp(fmtDTFull(Date.now()));
      } catch {
        if (!alive) return;
        setSnap(null); // 查询失败按空态展示（范围档白名单外等异常）
        setStamp(null);
      } finally {
        if (alive) setLoaded(true);
      }
    })();
    return () => {
      alive = false;
    };
  }, [range, agent, project, model, provider, refreshTick]);

  // 图表主题色（M1-4）：轴线/图例文字、网格线随主题切换；
  // 序列配色（STACK/热力色阶）为高饱和色，双主题通用
  const axisText = { color: theme === "dark" ? "#9ca3af" : "#57606a" };
  const splitLine = theme === "dark" ? "rgba(255,255,255,0.06)" : "rgba(0,0,0,0.08)";

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
    const grid = { left: 56, right: 16, top: 32, bottom: 24 };
    const xAxis = { type: "category" as const, data: labels, axisLabel: axisText };
    const mkYAxis = (fmt: (v: number) => string) => ({
      type: "value" as const,
      axisLabel: { ...axisText, formatter: fmt },
      splitLine: { lineStyle: { color: splitLine } },
    });
    // 时长模式：单柱（毫秒），y 轴短格式；无时长数据的桶计 0。
    // 青色 #22d3ee：避免与堆叠图「缓存写」黄同色不同义（2026-09-24 C1）
    if (trendMetric === "duration") {
      return {
        tooltip: {
          trigger: "axis",
          valueFormatter: (v: number) => fmtDuration(v),
        },
        grid,
        xAxis,
        yAxis: mkYAxis(fmtAxisDur),
        series: [
          {
            name: "生成时长",
            type: "bar",
            data: keys.map((k) => byBucket.get(k)?.duration_ms ?? 0),
            itemStyle: { color: "#22d3ee" },
            barMaxWidth: 26,
          },
        ],
      };
    }
    if (trendMetric === "calls") {
      return {
        tooltip: { trigger: "axis" },
        grid,
        xAxis,
        yAxis: mkYAxis((v) => fmtTokens(v)),
        series: [
          {
            name: "调用次数",
            type: "bar",
            data: keys.map((k) => byBucket.get(k)?.calls ?? 0),
            itemStyle: { color: "#38bdf8" },
            barMaxWidth: 26,
          },
        ],
      };
    }
    // token 模式：堆叠四项 + tooltip 补思考与合计行（合计=账单口径四项；思考单列不混入）
    return {
      tooltip: {
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
          const sum = arr.reduce((acc, p) => acc + (p.value || 0), 0);
          let html = `${arr[0].axisValue}<br/>${lines.join("<br/>")}<br/>合计　${fmtTokens(sum)}`;
          const row = byBucket.get(keys[arr[0].dataIndex]);
          if (row && row.reasoning > 0) html += `<br/>思考　${fmtTokens(row.reasoning)}`;
          return html;
        },
      },
      legend: { data: STACK.map((s) => s.name), textStyle: axisText, top: 0 },
      grid,
      xAxis,
      yAxis: mkYAxis((v) => fmtTokens(v)),
      series: STACK.map((s) => ({
        name: s.name,
        type: "bar",
        stack: "total",
        data: keys.map((k) => byBucket.get(k)?.[s.key] ?? 0),
        itemStyle: { color: s.color },
        barMaxWidth: 26,
      })),
    };
  }, [snap, range, trendMetric, theme]);

  // 额度消耗历史：按 供应商+窗口 分组成多条百分比曲线（5h 蓝锯齿／周窗口紫爬升）；
  // x 轴时间值轴，y 轴固定 0–100%；Rust 侧已抽稀，前端直接画
  const quotaOption = useMemo<EChartsCoreOption>(() => {
    const pts = snap?.quota_curve ?? [];
    const groups = new Map<string, QuotaPoint[]>();
    for (const p of pts) {
      const k = `${p.provider}:${p.window_kind}`;
      const list = groups.get(k);
      if (list) list.push(p);
      else groups.set(k, [p]);
    }
    return {
      tooltip: {
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
      legend: { textStyle: axisText, top: 0 },
      grid: { left: 44, right: 16, top: 30, bottom: 24 },
      xAxis: { type: "time", axisLabel: axisText },
      yAxis: {
        min: 0,
        max: 100,
        axisLabel: { ...axisText, formatter: "{value}%" },
        splitLine: { lineStyle: { color: splitLine } },
      },
      series: [...groups.entries()].map(([key, list]) => {
        const kind = list[0].window_kind;
        return {
          name: `${providerLabel(list[0].provider)} ${windowLabel(kind)}`,
          type: "line",
          showSymbol: false,
          data: list.map((p) => [p.fetched_at, p.used_percent]),
          itemStyle: { color: QUOTA_COLORS[kind] ?? hashColor(key) },
          lineStyle: { width: 1.5 },
        };
      }),
    };
  }, [snap, theme]);

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
        inRange: { color: ["#1e3a5f", "#38bdf8", "#34d399"] },
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
  const hasFilter = agent != null || project != null || model != null || provider != null;

  /** 会话数卡跳转会话窗口（跨窗口动线：报表看总量，明细去会话） */
  const openSessions = () => {
    invoke("show_sessions_window").catch(() => {});
  };

  return (
    <div className="rp-root">
      <div className="rp-header">
        <span className="rp-title">用量报表</span>
        <div className="rp-header-right">
          {stamp && (
            <Tip content="打开/刷新时查询本地数据库的时间点（不做自动轮询）">
              <span className="rp-stamp">数据截至 {stamp.slice(5)}</span>
            </Tip>
          )}
          <button className="rp-btn" onClick={() => setRefreshTick((t) => t + 1)}>
            刷新
          </button>
          {/* 会话级导出已随会话明细迁往会话窗口（M1-10） */}
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
        </div>
      </div>

      {empty ? (
        <div className="rp-empty">
          {hasFilter
            ? "当前筛选条件下暂无数据，可清除筛选或放宽时间范围"
            : "暂无数据——请先在设置页勾选要监控的 Agent，数据会随使用自动积累"}
        </div>
      ) : (
        snap && (
          <>
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
              const cards: {
                key: string;
                label: string;
                val: string;
                tip?: string;
                delta?: React.ReactNode;
                warn?: boolean;
                link?: boolean;
                onClick?: () => void;
              }[] = [
                {
                  key: "tokens",
                  label: "总 Token",
                  val: fmtTokens(s.total_tokens),
                  tip: "统计口径：输入＋输出＋缓存读写的全量 token（与官方账单同口径）",
                  delta: <Delta value={s.total_tokens} prev={prev?.total_tokens ?? null} />,
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
                  tip: "模型生成时长合计；仅 ZCode 等转录含时长字段的 Agent 有数据，无则显示 —",
                  delta: <Delta value={s.duration_ms} prev={prev?.duration_ms ?? null} />,
                },
                {
                  key: "ttft",
                  label: "平均首字",
                  val: s.ttft_avg_ms != null ? fmtMs(s.ttft_avg_ms) : "—",
                  tip: "首字延迟（TTFT）按调用平均；仅含上报该字段的调用。分模型对比见下方维度区",
                  delta: <Delta value={s.ttft_avg_ms} prev={prev?.ttft_avg_ms ?? null} />,
                },
                {
                  key: "reason",
                  label: "思考占比",
                  val: ratio,
                  tip: "思考占比＝思考／（输入＋输出＋思考），缓存读写不计入；极小占比显示 <1%",
                },
                {
                  key: "cache",
                  label: "缓存命中率",
                  val: hit != null ? `${hit}%` : "—",
                  tip: "缓存命中率＝缓存读／（输入＋缓存读）；命中越高，重复上下文的生成越省时省额度",
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
                      onClick={c.onClick}
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
                      <div className="rp-summ-val">{c.val}</div>
                      {c.delta}
                    </div>
                  ))}
                </div>
              );
            })()}

            {/* 筛选条：可搜索下拉（与维度条点击双向联动） */}
            <div className="rp-filter">
              <SearchSelect
                value={agent}
                allLabel="全部 Agent"
                onChange={setAgent}
                options={snap.options.agents.map((a) => ({ value: a, label: a }))}
              />
              <SearchSelect
                value={project}
                allLabel="全部项目"
                onChange={setProject}
                options={snap.options.projects.map((p) => ({
                  value: p,
                  label: tail(p) || "未知项目",
                }))}
              />
              <SearchSelect
                value={model}
                allLabel="全部模型"
                onChange={setModel}
                options={snap.options.models.map((m) => ({ value: m, label: m }))}
              />
              <SearchSelect
                value={provider}
                allLabel="全部供应商"
                onChange={setProvider}
                options={snap.options.providers.map((p) => ({
                  value: p,
                  label: p === "unknown" ? "未知供应商" : providerLabel(p),
                }))}
              />
              {hasFilter && (
                <button
                  className="rp-btn rp-clear"
                  onClick={() => {
                    setAgent(null);
                    setProject(null);
                    setModel(null);
                    setProvider(null);
                  }}
                >
                  清除筛选
                </button>
              )}
            </div>

            {/* 主区：趋势（左）+ 维度面板（右，点击条目联动全局筛选） */}
            <div className="rp-main">
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
                  <MetricToggle
                    value={trendMetric}
                    onChange={setTrendMetric}
                    options={[
                      { key: "token", label: "Token" },
                      { key: "calls", label: "次数" },
                      { key: "duration", label: "时长" },
                    ]}
                  />
                </div>
                <Chart option={trendOption} height={300} />
              </section>
              <section className="rp-card rp-dims">
                <div className="rp-dims-head">
                  <span className="rp-dims-title">维度分布</span>
                  <MetricToggle
                    value={dimMetric}
                    onChange={(v) => setDimMetric(v === "calls" ? "calls" : "token")}
                    options={[
                      { key: "token", label: "Token" },
                      { key: "calls", label: "次数" },
                    ]}
                  />
                </div>
                <DimBars
                  title="按 Agent"
                  items={snap.by_agent}
                  active={agent}
                  onSelect={setAgent}
                  colorFn={agentColor}
                  metric={dimMetric}
                />
                <DimBars
                  title={`按项目（${snap.by_project.length}）`}
                  items={snap.by_project}
                  active={project}
                  onSelect={setProject}
                  fmt={(l) => tail(l) || "未知项目"}
                  metric={dimMetric}
                />
                <DimBars
                  title="按模型"
                  items={snap.by_model}
                  active={model}
                  onSelect={setModel}
                  metric={dimMetric}
                />
                {/* 按模型首字延迟：条长=平均首字（降序），点击同样联动模型筛选 */}
                {snap.ttft_by_model.length > 0 && (
                  <DimBars
                    title="按模型首字延迟"
                    items={snap.ttft_by_model}
                    active={model}
                    onSelect={setModel}
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
                  active={provider}
                  onSelect={setProvider}
                  fmt={(l) => (l === "unknown" ? "未知供应商" : providerLabel(l))}
                  metric={dimMetric}
                />
                {/* 错误分布：label 是原始 error_type，中文归大类展示；条长=次数（红系语义色） */}
                {snap.by_error.length > 0 && (
                  <DimBars
                    title="按错误类型"
                    items={snap.by_error}
                    active={null}
                    onSelect={() => {}}
                    selectable={false}
                    fmt={errorReason}
                    colorFn={() => "var(--danger)"}
                    fmtVal={(t) => t.toLocaleString()}
                    tipFormat={(it, name) => `${name} · ${it.total.toLocaleString()} 次`}
                    metric={dimMetric}
                  />
                )}
              </section>
            </div>

            {/* 额度消耗历史（quota_snapshots 采样曲线；未配置额度整卡隐藏） */}
            {snap.quota_curve.length > 0 && (
              <section className="rp-card">
                <div className="rp-card-head">
                  <div className="rp-card-title">
                    额度消耗历史
                    <Tip content="按采集周期（约 5 分钟）采样的供应商额度用量百分比：5h 窗口呈锯齿（用满→重置），周窗口缓慢爬升；未配置额度的供应商不显示">
                      <span className="rp-info">
                        <InfoIcon />
                      </span>
                    </Tip>
                  </div>
                </div>
                <Chart option={quotaOption} height={190} />
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
              <Chart option={heatOption} height={230} />
            </section>

            {/* 会话明细已迁往独立会话窗口（M1-10）：本页收敛为纯聚合分析 */}

            <div className="rp-foot">
              统计口径细节见各卡片 ⓘ · 数据源为本地 SQLite，不代表官方计费 ·
              逐会话明细请用岛面板或托盘菜单打开「会话」窗口
            </div>
          </>
        )
      )}
    </div>
  );
}
