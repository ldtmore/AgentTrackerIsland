/**
 * 报表页（M1-R1 重构；M1-10 瘦身：会话明细迁往独立会话窗口，本页回归纯聚合分析）：
 * 汇总卡 / 趋势（粒度随范围自动：今日→小时、7~30 天→日、90 天→周、全部→月）/
 * Agent·项目·模型·供应商维度条（点击联动全局筛选）/ 周×小时热力图。
 * 数据来自本地 SQLite，Rust 侧单命令整页快照（各图口径一致）；
 * 时间口径为本机时区；筛选变更全量重拉（本地查询毫秒级）
 */
import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import * as echarts from "echarts/core";
import type { EChartsCoreOption } from "echarts/core";
import { BarChart, HeatmapChart } from "echarts/charts";
import {
  GridComponent,
  LegendComponent,
  TooltipComponent,
  VisualMapComponent,
} from "echarts/components";
import { CanvasRenderer } from "echarts/renderers";
import Chart from "./Chart";
import SearchSelect from "../shared/SearchSelect";
import { tail } from "../shared/format";
import { fmtTokens, fmtDuration, agentColor, errorReason } from "../shared/types";
import { useTheme } from "../shared/theme";
import "./report.css";

// 按需注册用到的图表与组件（01-RESEARCH §9，减小 bundle）
echarts.use([
  BarChart,
  HeatmapChart,
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
  projects: number;
  errors: number;
  duration_ms: number | null;
  ttft_avg_ms: number | null;
  reasoning_tokens: number;
  billable_tokens: number;
}

/** 单维度分组行（token 总量 + 调用次数；错误分布里 total/calls 同为次数） */
interface SliceUsage {
  label: string;
  total: number;
  calls: number;
}

/** 趋势行（时间桶 + 四项用量 + 次数 + 生成时长，指标切换免回查） */
interface TrendRow {
  bucket: string;
  input: number;
  output: number;
  cache_read: number;
  cache_creation: number;
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

/** 筛选下拉选项（projects 中空串=未知项目） */
interface FilterOptions {
  agents: string[];
  projects: string[];
  models: string[];
}

/** 整页快照 */
interface ReportSnapshot {
  summary: SummaryStats;
  trend: TrendRow[];
  by_agent: SliceUsage[];
  by_project: SliceUsage[];
  by_model: SliceUsage[];
  by_provider: SliceUsage[];
  by_error: SliceUsage[];
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

/** 维度条渐变备用色（项目/模型/供应商行，未提供 colorFn 时按序取用） */
const DIM_COLORS = ["#38bdf8", "#f472b6", "#facc15", "#4ade80", "#fb7185", "#a78bfa", "#60a5fa", "#34d399"];

/** 趋势桶显示标签：今日→"HH:00"、7/30 天与 90 天→"MM-DD"、全部→"YYYY-MM" */
function bucketLabel(b: string, range: string): string {
  if (range === "today") return b.slice(11);
  if (range === "all") return b;
  return b.slice(5);
}

/** 可搜索下拉（筛选用）已抽至 shared/SearchSelect（M1-10：报表页与会话窗口共用） */

/** 指标键（趋势图三档；热力图两档无时长数据） */type MetricKey = "token" | "calls" | "duration";

/** 毫秒 → "900 毫秒"/"1.2 秒"（平均首字延迟用） */
function fmtMs(ms: number): string {
  return ms < 1000 ? `${ms} 毫秒` : `${(ms / 1000).toFixed(1)} 秒`;
}

/** 毫秒 → 短格式 "45s"/"12m"/"1.3h"（趋势图 y 轴用，长格式会挤爆轴标签） */
function fmtAxisDur(ms: number): string {
  if (ms < 60_000) return `${Math.round(ms / 1000)}s`;
  if (ms < 3_600_000) return `${Math.round(ms / 60_000)}m`;
  return `${(ms / 3_600_000).toFixed(1)}h`;
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

/** 维度比例条（HTML 自绘，点击条目=全局筛选该维度，再点取消；不占 ECharts 实例） */
function DimBars({
  title,
  items,
  active,
  onSelect,
  colorFn,
  fmt,
  selectable = true,
}: {
  title: string;
  items: SliceUsage[];
  /** 当前生效的筛选值（null=未筛选） */
  active: string | null;
  onSelect: (v: string | null) => void;
  /** 行颜色（缺省按序取 DIM_COLORS） */
  colorFn?: (label: string) => string;
  /** 标签显示转换（如项目路径→尾段；过滤值仍用原值） */
  fmt?: (label: string) => string;
  /** 是否可点击筛选（供应商维度暂无全局筛选，仅展示） */
  selectable?: boolean;
}) {
  const max = Math.max(...items.map((i) => i.total), 1);
  const shown = items.slice(0, 8);
  return (
    <div className="rp-dim">
      <div className="rp-dim-title">{title}</div>
      {shown.length === 0 ? (
        <div className="rp-dim-empty">无数据</div>
      ) : (
        shown.map((it, idx) => {
          const on = selectable && active === it.label;
          const name = fmt ? fmt(it.label) : it.label;
          return (
            <div
              key={it.label}
              className={`rp-dim-row${on ? " on" : ""}${selectable ? "" : " flat"}`}
              title={`${name} · ${fmtTokens(it.total)} token · ${it.calls} 次调用${selectable ? `（点击${on ? "取消" : ""}筛选）` : ""}`}
              onClick={() => selectable && onSelect(on ? null : it.label)}
            >
              <span className="rp-dim-name" title={name}>
                {name}
              </span>
              <span className="rp-dim-track">
                <span
                  className="rp-dim-fill"
                  style={{
                    width: `${(it.total / max) * 100}%`,
                    background: colorFn ? colorFn(it.label) : DIM_COLORS[idx % DIM_COLORS.length],
                  }}
                />
              </span>
              <span className="rp-dim-val">{fmtTokens(it.total)}</span>
            </div>
          );
        })
      )}
      {items.length > shown.length && (
        <div className="rp-dim-more">其余 {items.length - shown.length} 项</div>
      )}
    </div>
  );
}

export default function Report() {
  const theme = useTheme();
  // 筛选状态：范围档 + 三维度（null=全部；项目空串=未知项目）
  const [range, setRange] = useState("30d");
  const [agent, setAgent] = useState<string | null>(null);
  const [project, setProject] = useState<string | null>(null);
  const [model, setModel] = useState<string | null>(null);
  // 数据：整页快照（会话明细已迁往会话窗口，M1-10）
  const [snap, setSnap] = useState<ReportSnapshot | null>(null);
  const [loaded, setLoaded] = useState(false);
  // 图表指标切换（数据已多指标下发，切换免回查）
  const [trendMetric, setTrendMetric] = useState<MetricKey>("token");
  const [heatMetric, setHeatMetric] = useState<"token" | "calls">("token");

  // 筛选变更：整页快照重拉（本地毫秒级）
  useEffect(() => {
    let alive = true;
    setLoaded(false);
    (async () => {
      try {
        const s = await invoke<ReportSnapshot>("report_snapshot", { range, agent, project, model });
        if (!alive) return;
        setSnap(s);
      } catch {
        if (alive) setSnap(null); // 查询失败按空态展示（范围档白名单外等异常）
      } finally {
        if (alive) setLoaded(true);
      }
    })();
    return () => {
      alive = false;
    };
  }, [range, agent, project, model]);

  // 图表主题色（M1-4）：轴线/图例文字、网格线随主题切换；
  // 序列配色（STACK/DIM_COLORS/热力色阶）为高饱和色，双主题通用
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
    // 时长模式：单柱（毫秒），y 轴短格式；无时长数据的桶计 0
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
            itemStyle: { color: "#fbbf24" },
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
    return {
      tooltip: { trigger: "axis" },
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

  // 热力图：列=小时，行=星期，色阶随指标切换
  const heatOption = useMemo<EChartsCoreOption>(() => {
    const heat = snap?.heatmap ?? [];
    const val = (c: HeatCell) => (heatMetric === "calls" ? c.calls : c.total);
    const max = Math.max(1, ...heat.map(val));
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
  const empty = loaded && (snap == null || (summary != null && summary.calls === 0));
  const hasFilter = agent != null || project != null || model != null;

  return (
    <div className="rp-root">
      <div className="rp-header">
        <span className="rp-title">用量报表</span>
        <div className="rp-header-right">
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
        <div className="rp-empty">所选条件下暂无数据</div>
      ) : (
        snap && (
          <>
            {/* 汇总卡行：第一眼看清总量、强度与效率（时长/首字为 null 显示 — 降级） */}
            {(() => {
              const s = snap.summary;
              // 思考占比：思考 /（输入+输出+思考），缓存读写不计入（脚注口径）
              const denom = s.reasoning_tokens + s.billable_tokens;
              const cards: { label: string; val: string; warn?: boolean }[] = [
                { label: "总 Token", val: fmtTokens(s.total_tokens) },
                { label: "调用次数", val: s.calls.toLocaleString() },
                { label: "生成时长", val: fmtDuration(s.duration_ms) },
                { label: "平均首字", val: s.ttft_avg_ms != null ? fmtMs(s.ttft_avg_ms) : "—" },
                {
                  label: "思考占比",
                  // 极小占比四舍五入为 0% 时显示 <1%（真实数据常见 0.3% 一类）
                  val:
                    denom > 0
                      ? (s.reasoning_tokens / denom) * 100 < 1
                        ? "<1%"
                        : `${Math.round((s.reasoning_tokens / denom) * 100)}%`
                      : "—",
                },
                { label: "会话数", val: s.sessions.toLocaleString() },
                { label: "活跃项目", val: s.projects.toLocaleString() },
                {
                  label: "出错次数",
                  val: s.errors.toLocaleString(),
                  warn: s.errors > 0,
                },
              ];
              return (
                <div className="rp-cards">
                  {cards.map((c) => (
                    <div key={c.label} className={`rp-summ${c.warn ? " warn" : ""}`}>
                      <div className="rp-summ-label">{c.label}</div>
                      <div className="rp-summ-val">{c.val}</div>
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
              {hasFilter && (
                <button
                  className="rp-btn rp-clear"
                  onClick={() => {
                    setAgent(null);
                    setProject(null);
                    setModel(null);
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
                <DimBars
                  title="按 Agent"
                  items={snap.by_agent}
                  active={agent}
                  onSelect={setAgent}
                  colorFn={agentColor}
                />
                <DimBars
                  title="按项目"
                  items={snap.by_project}
                  active={project}
                  onSelect={setProject}
                  fmt={(l) => tail(l) || "未知项目"}
                />
                <DimBars title="按模型" items={snap.by_model} active={model} onSelect={setModel} />
                <DimBars
                  title="按供应商"
                  items={snap.by_provider}
                  active={null}
                  onSelect={() => {}}
                  selectable={false}
                />
                {/* 错误分布：label 是原始 error_type，中文归大类展示；条长=次数 */}
                {snap.by_error.length > 0 && (
                  <DimBars
                    title="按错误类型"
                    items={snap.by_error}
                    active={null}
                    onSelect={() => {}}
                    selectable={false}
                    fmt={errorReason}
                  />
                )}
              </section>
            </div>

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
              统计口径：输入＋输出＋缓存读写的全量 token（与官方账单同口径）·
              思考占比＝思考／（输入＋输出＋思考），不含缓存 ·
              生成时长与平均首字仅 ZCode 等转录含时长字段的 Agent ·
              逐会话明细请用岛面板或托盘菜单打开「会话」窗口 ·
              数据源为本地 SQLite，不代表官方计费
            </div>
          </>
        )
      )}
    </div>
  );
}
