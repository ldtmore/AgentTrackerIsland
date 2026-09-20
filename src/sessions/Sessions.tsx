/**
 * 会话窗口（M1-10，#sessions 路由）：全量会话管理中心——
 * 所有 Agent 所有会话的分页浏览：关键字搜索（标题/项目路径）、
 * 状态档（全部/进行中/已结束/有错误）、范围＋Agent/项目/模型筛选、
 * 四键排序（最近活动/总Token/次数/时长）、行点击跳转对应窗口（仅近期活跃）、
 * CSV 导出（所见即所得：当前筛选＋排序的全量）。
 * 数据源=本地 SQLite（Rust 侧阻塞线程池单命令查询，毫秒级）；
 * 状态口径与岛面板同源（shared/sessionDisplay）；
 * 手动刷新＋「数据截至」时间戳诚实呈现数据时点，不做自动轮询
 */
import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { save } from "@tauri-apps/plugin-dialog";
import SearchSelect from "../shared/SearchSelect";
import {
  cardTitle,
  displayState,
  AGENT_BADGE,
  toSessionState,
} from "../shared/sessionDisplay";
import { fmtDT, fmtDTFull, fmtHMS, daySepLabel, fmtMs, tail } from "../shared/format";
import {
  AGENT_DEFS,
  agentColor,
  errorReason,
  fmtDuration,
  fmtRelative,
  fmtTokens,
} from "../shared/types";
import Tip from "../shared/Tip";
import { useTheme } from "../shared/theme";
import { useAgentColors } from "../shared/useAgentColors";
import "./sessions.css";

// ===== 与 Rust store session_page 结构对应的类型 =====

/** 会话中心行（逐会话聚合；state 为聚合器最后已知状态，可能缺失） */
interface SessionRow {
  session_id: string;
  agent: string;
  state: string | null;
  model: string | null;
  project_dir: string | null;
  title: string | null;
  first_ts: number;
  last_ts: number;
  calls: number;
  total_tokens: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  reasoning_tokens: number;
  duration_ms: number | null;
  ttft_avg_ms: number | null;
  errors: number;
  error_types: string | null;
}

/** 会话详情：单次调用行（抽屉「调用流水」） */
interface SessionCallRow {
  ts: number;
  model: string | null;
  input_tokens: number;
  output_tokens: number;
  reasoning_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  duration_ms: number | null;
  ttft_ms: number | null;
  error_type: string | null;
}

/** 会话详情：状态事件行（抽屉「状态时间线」） */
interface SessionEventRow {
  ts: number;
  hook: string | null;
  payload: string | null;
}

/** 会话详情包（total 为库中真实总数：列表受上限截断时用于诚实展示「N / 共 M」） */
interface SessionDetail {
  calls: SessionCallRow[];
  events: SessionEventRow[];
  calls_total: number;
  events_total: number;
}

/** 会话分页结果 */
interface SessionPage {
  total: number;
  page_size: number;
  rows: SessionRow[];
}

/** 筛选下拉选项（projects 中空串=未知项目） */
interface FilterOptions {
  agents: string[];
  projects: string[];
  models: string[];
}

/** 范围档（与 Rust build_filter 白名单同步；默认今日——所有者拍板） */
const RANGES = [
  { key: "today", label: "今日" },
  { key: "7d", label: "近 7 天" },
  { key: "30d", label: "近 30 天" },
  { key: "90d", label: "近 90 天" },
  { key: "all", label: "全部" },
];

/** 状态档（与 Rust session_page 白名单同步） */
const STATUS: { key: string; label: string; tip: string }[] = [
  { key: "all", label: "全部", tip: "不过滤状态" },
  { key: "active", label: "进行中", tip: "工作中/等待输入/出错，或 2 小时内仍空闲" },
  { key: "ended", label: "已结束", tip: "进程已退出，或空闲超过 2 小时" },
  { key: "errored", label: "有错误", tip: "范围内出现过出错的调用" },
];

/** 排序键（与 Rust session_page 白名单同步；方向固定降序，升序对管理场景无意义） */
const SORTS: Record<string, string> = {
  recent: "最近",
  tokens: "Token",
  calls: "次数",
  duration: "时长",
};

/** 行点击跳转的会话新鲜度门槛：超过该时长的会话进程大概率已退出，
 *  跳转必然未命中——不可点击，避免「点了没反应」的负体验 */
const JUMPABLE_MS = 30 * 60_000;

/** 每页行数档位（分页器下拉选项；store 侧钳制 ≤200） */
const PAGE_SIZES = [20, 50, 100];

/** 会话行派生指标：平均单次 token／思考占比（口径与报表同源，不含缓存）；
 *  主表格悬浮明细与抽屉统计条共用，避免同一段口径两处漂移 */
function deriveRowMetrics(r: SessionRow) {
  const avgPerCall = r.calls > 0 ? Math.round(r.total_tokens / r.calls) : 0;
  const denom = r.reasoning_tokens + r.input_tokens + r.output_tokens;
  const thinkPct =
    r.reasoning_tokens > 0 && denom > 0
      ? (r.reasoning_tokens / denom) * 100 < 1
        ? "<1%"
        : `${Math.round((r.reasoning_tokens / denom) * 100)}%`
      : null;
  return { avgPerCall, thinkPct };
}

/** 流水列表按自然日插分隔行：同日连续项共用一个标签（今天／昨天／MM-dd（周X））；
 *  sep 非空为分隔行、item 非空为数据行，二者互斥 */
function withDaySeps<T extends { ts: number }>(items: T[]): Array<{ sep?: string; item?: T }> {
  const out: Array<{ sep?: string; item?: T }> = [];
  let lastDay = "";
  for (const it of items) {
    const label = daySepLabel(it.ts);
    if (label !== lastDay) {
      out.push({ sep: label });
      lastDay = label;
    }
    out.push({ item: it });
  }
  return out;
}

/** 调用条目复制文本：完整时间＋键值对精确数值（页面显示用缩写值，
 *  复制用精确值——贴给 AI/issue 可直接解析计算） */
function buildCallCopyText(c: SessionCallRow): string {
  let s =
    `${fmtDTFull(c.ts)} · model=${c.model ?? "--"} · input=${c.input_tokens}` +
    ` · output=${c.output_tokens} · reasoning=${c.reasoning_tokens}` +
    ` · cache_read=${c.cache_read_tokens} · cache_write=${c.cache_creation_tokens}`;
  if (c.duration_ms != null) s += ` · duration_ms=${c.duration_ms}`;
  if (c.ttft_ms != null) s += ` · ttft_ms=${c.ttft_ms}`;
  if (c.error_type) s += ` · error=${c.error_type}`;
  return s;
}

/** 状态事件复制文本：时间＋hook＋payload 原文 */
function buildEventCopyText(e: SessionEventRow): string {
  return `${fmtDTFull(e.ts)} · hook=${e.hook ?? "未知"} · payload=${e.payload ?? ""}`;
}

export default function Sessions() {
  useTheme(); // 深浅主题跟随（与其他窗口同一套 Hook）
  useAgentColors(); // 注入设置页自定义的 Agent 身份色（徽标/维度配色跟随），变更实时生效
  // 筛选状态：范围（默认今日）＋状态档＋三维度（null=全部）＋关键字（防抖后生效）
  const [range, setRange] = useState("today");
  const [status, setStatus] = useState("all");
  const [sort, setSort] = useState("recent");
  const [agent, setAgent] = useState<string | null>(null);
  const [project, setProject] = useState<string | null>(null);
  const [model, setModel] = useState<string | null>(null);
  const [search, setSearch] = useState(""); // 输入框实时值
  const [keyword, setKeyword] = useState(""); // 防抖后真正参与查询的词
  const [pageSize, setPageSize] = useState(20); // 每页行数（分页器下拉）
  // 数据：分页结果 + 下拉选项 + 加载/错误态 + 数据截至时间戳
  const [page, setPage] = useState<SessionPage | null>(null);
  const [options, setOptions] = useState<FilterOptions | null>(null);
  const [offset, setOffset] = useState(0);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [stamp, setStamp] = useState("");
  // CSV 导出提示：text 为展示文案；path 非空时可点击定位到文件
  const [exportTip, setExportTip] = useState<{ text: string; path: string | null } | null>(null);
  // 会话详情抽屉：当前查看的行 + 流水/时间线数据（null=未打开）+ 加载失败原因
  const [detail, setDetail] = useState<{
    row: SessionRow;
    data: SessionDetail | null;
    error?: string;
  } | null>(null);
  // 抽屉内容页签：calls=调用流水，events=状态时间线
  const [detailTab, setDetailTab] = useState<"calls" | "events">("calls");
  // 抽屉流水方向：desc=最新在上（默认），asc=最早在上；关闭抽屉不重置，保持用户偏好
  const [detailOrder, setDetailOrder] = useState<"desc" | "asc">("desc");
  // 刚复制成功的流水条目标识（显示「已复制」1.2 秒后还原）
  const [copiedKey, setCopiedKey] = useState<string | null>(null);

  // 关键字 300ms 防抖：本地查询虽快，连续击键也不必每键一查
  useEffect(() => {
    const t = setTimeout(() => setKeyword(search.trim()), 300);
    return () => clearTimeout(t);
  }, [search]);

  /** 拉取一页（o=偏移）；筛选变更拉第一页，翻页拉目标页 */
  const fetchPage = useCallback(
    async (o: number) => {
      try {
        const p = await invoke<SessionPage>("session_page", {
          range,
          agent,
          project,
          model,
          status,
          keyword,
          sort,
          pageSize,
          offset: o,
        });
        setPage(p);
        setError(null);
        setStamp(fmtDTFull(Date.now()));
      } catch (e) {
        setError(String(e));
      } finally {
        setLoaded(true);
      }
    },
    [range, agent, project, model, status, keyword, sort, pageSize],
  );

  // 筛选/搜索/排序变更：回到第一页重拉（下拉选项只随范围变化，顺带刷新）
  useEffect(() => {
    setLoaded(false);
    setOffset(0);
    void fetchPage(0);
    invoke<FilterOptions>("session_options", { range })
      .then(setOptions)
      .catch(() => {}); // 选项拉取失败不打扰：下拉 retains 上一次内容
  }, [range, status, agent, project, model, keyword, sort, fetchPage]);

  /** 翻页：只重拉会话表 */
  const turnPage = (dir: 1 | -1) => {
    const size = page?.page_size ?? 20;
    const total = page?.total ?? 0;
    const pages = Math.max(1, Math.ceil(total / size));
    const next = Math.min(Math.max(offset + dir * size, 0), (pages - 1) * size);
    if (next === offset) return;
    setOffset(next);
    void fetchPage(next);
  };

  /** 手动刷新：原地重拉当前页（数据时点诚实呈现于「数据截至」） */
  const refresh = () => void fetchPage(offset);

  /** 导出当前筛选＋排序的全量会话 CSV：先弹系统保存对话框（取消则不动作），
   *  成功后提示文字可点击定位文件 */
  const doExport = async () => {
    const d = new Date();
    const p2 = (n: number) => String(n).padStart(2, "0");
    const stampText = `${d.getFullYear()}${p2(d.getMonth() + 1)}${p2(d.getDate())}-${p2(d.getHours())}${p2(d.getMinutes())}${p2(d.getSeconds())}`;
    const target = await save({
      title: "导出会话列表 CSV",
      defaultPath: `去你的岛-会话列表-${stampText}.csv`,
      filters: [{ name: "CSV 文件", extensions: ["csv"] }],
    });
    if (!target) return; // 用户取消
    setExportTip({ text: "导出中…", path: null });
    try {
      await invoke<string>("export_sessions_csv", {
        range,
        agent,
        project,
        model,
        status,
        keyword,
        sort,
        path: target,
      });
      setExportTip({
        text: `已导出：${target.split(/[\\/]/).pop()}（点击打开所在目录）`,
        path: target,
      });
    } catch (e) {
      setExportTip({ text: `导出失败：${e}`, path: null });
    }
  };

  /** 点击导出提示：资源管理器定位到导出文件 */
  const openExportLocation = () => {
    if (!exportTip?.path) return;
    invoke("open_file_location", { path: exportTip.path }).catch((e) => {
      setExportTip({ text: `打开目录失败：${e}`, path: null });
    });
  };

  /** 拉取指定会话的调用流水与状态时间线（openDetail 与抽屉内刷新共用）；
   *  快速切换行时丢弃过期响应（只认当前查看的会话） */
  const fetchDetail = (sessionId: string) => {
    invoke<SessionDetail>("session_detail", { sessionId })
      .then((d) =>
        setDetail((cur) =>
          cur && cur.row.session_id === sessionId ? { ...cur, data: d, error: undefined } : cur,
        ),
      )
      .catch((e) =>
        setDetail((cur) =>
          cur && cur.row.session_id === sessionId
            ? {
                ...cur,
                data: { calls: [], events: [], calls_total: 0, events_total: 0 },
                error: String(e),
              }
            : cur,
        ),
      );
  };

  /** 行点击打开详情抽屉 */
  const openDetail = (row: SessionRow) => {
    setDetail({ row, data: null });
    fetchDetail(row.session_id);
  };

  /** 抽屉内刷新：重新拉当前会话的流水与时间线（汇总条保持打开时的快照，诚实不联动） */
  const refreshDetail = () => {
    if (!detail || detail.data == null) return;
    setDetail({ ...detail, data: null, error: undefined });
    fetchDetail(detail.row.session_id);
  };

  /** 复制流水条目到剪贴板：成功后该条按钮瞬时显示「已复制」；
   *  写入失败静默忽略（观测工具，复制是辅助动作） */
  const copyEntry = (key: string, text: string) => {
    navigator.clipboard
      .writeText(text)
      .then(() => {
        setCopiedKey(key);
        setTimeout(() => setCopiedKey((cur) => (cur === key ? null : cur)), 1200);
      })
      .catch(() => {});
  };

  /** 抽屉内跳转：仅新鲜会话可跳（复用 T10 窗口匹配），未命中静默失败 */
  const jump = (r: SessionRow) => {
    if (Date.now() - r.last_ts > JUMPABLE_MS) return;
    invoke("focus_session", { sessionId: r.session_id }).catch(() => {});
  };

  // Esc 关闭详情抽屉
  useEffect(() => {
    if (!detail) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setDetail(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [detail]);

  /** 可排序表头单元格：点击切换排序键，当前键带 ▼ 指示（顺序须与数据行一致） */
  const sortTh = (k: keyof typeof SORTS) => (
    <th key={k} className="sortable" onClick={() => setSort(k)}>
      {SORTS[k]}
      {sort === k && <span className="ss-sort-mark"> ▼</span>}
    </th>
  );

  const rows = page?.rows ?? [];
  // 服务端回显的每页行数（权威值）——与用户选择的 pageSize 状态区分命名
  const effectivePageSize = page?.page_size ?? pageSize;
  const pageCount = Math.max(1, Math.ceil((page?.total ?? 0) / effectivePageSize));
  const pageNo = Math.floor(offset / effectivePageSize) + 1;
  const hasFilter = agent != null || project != null || model != null || keyword !== "" || status !== "all";
  const clearFilter = () => {
    setAgent(null);
    setProject(null);
    setModel(null);
    setStatus("all");
    setSearch("");
    setKeyword("");
  };

  // ===== 抽屉派生数据 =====
  // 流水方向翻转：Rust 固定倒序拉最近数据，正序显示时纯前端翻转，切换零等待
  const detailCalls = detail?.data
    ? detailOrder === "asc"
      ? [...detail.data.calls].reverse()
      : detail.data.calls
    : [];
  const detailEvents = detail?.data
    ? detailOrder === "asc"
      ? [...detail.data.events].reverse()
      : detail.data.events
    : [];
  // 统计条派生指标（口径与主表格 Tip 同源：含缓存；思考占比不含缓存）
  const dr = detail?.row ?? null;
  const drMetrics = dr ? deriveRowMetrics(dr) : null;
  // 页签计数文案：被上限截断时诚实展示「N / 共 M」
  const callsCountText = detail?.data
    ? detail.data.calls_total > detail.data.calls.length
      ? `${detail.data.calls.length} / 共 ${detail.data.calls_total}`
      : `${detail.data.calls.length}`
    : "…";
  const eventsCountText = detail?.data
    ? detail.data.events_total > detail.data.events.length
      ? `${detail.data.events.length} / 共 ${detail.data.events_total}`
      : `${detail.data.events.length}`
    : "…";

  return (
    <div className="ss-root">
      {/* 页头：标题 + 数据截至 + 刷新 + 导出 */}
      <div className="ss-header">
        <span className="ss-title">会话</span>
        <div className="ss-header-right">
          {exportTip && (
            <span
              className={`ss-export-tip${exportTip.path ? " link" : ""}`}
              title={exportTip.path ?? exportTip.text}
              onClick={openExportLocation}
            >
              {exportTip.text}
            </span>
          )}
          {stamp && (
            <Tip content="打开/刷新时查询本地数据库的时间点（不做自动轮询）">
              <span className="ss-stamp">数据截至 {stamp.slice(5)}</span>
            </Tip>
          )}
          <button className="ss-btn" onClick={refresh}>
            刷新
          </button>
          <button className="ss-btn ss-export" onClick={doExport}>
            导出 CSV
          </button>
        </div>
      </div>

      {/* 工具行 1：关键字搜索 + 状态档 */}
      <div className="ss-toolbar">
        <div className="ss-search">
          <input
            value={search}
            placeholder="搜索标题或项目路径…"
            onChange={(e) => setSearch(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") setSearch("");
            }}
          />
          {search && (
            <button className="ss-search-clear" title="清空" onClick={() => setSearch("")}>
              ×
            </button>
          )}
        </div>
        <div className="ss-chips">
          {STATUS.map((s) => (
            <Tip key={s.key} content={s.tip}>
              <button
                className={`ss-chip${status === s.key ? " on" : ""}`}
                onClick={() => setStatus(s.key)}
              >
                {s.label}
              </button>
            </Tip>
          ))}
        </div>
      </div>

      {/* 工具行 2：范围档 + 维度筛选 + 清除 */}
      <div className="ss-filter">
        <div className="ss-ranges">
          {RANGES.map((r) => (
            <button
              key={r.key}
              className={`ss-btn${range === r.key ? " active" : ""}`}
              onClick={() => setRange(r.key)}
            >
              {r.label}
            </button>
          ))}
        </div>
        <SearchSelect
          value={agent}
          allLabel="全部 Agent"
          onChange={setAgent}
          options={(options?.agents ?? []).map((a) => ({ value: a, label: a }))}
        />
        <SearchSelect
          value={project}
          allLabel="全部项目"
          onChange={setProject}
          options={(options?.projects ?? []).map((p) => ({
            value: p,
            label: tail(p) || "未知项目",
          }))}
        />
        <SearchSelect
          value={model}
          allLabel="全部模型"
          onChange={setModel}
          options={(options?.models ?? []).map((m) => ({ value: m, label: m }))}
        />
        {hasFilter && (
          <button className="ss-btn ss-clear" onClick={clearFilter}>
            清除筛选
          </button>
        )}
      </div>

      {/* 查询失败：行内提示 + 重试（不弹窗，红线⑤） */}
      {error ? (
        <div className="ss-empty">
          查询失败：{error}
          <button className="ss-btn ss-retry" onClick={refresh}>
            重试
          </button>
        </div>
      ) : !loaded ? (
        <div className="ss-empty">加载中…</div>
      ) : rows.length === 0 ? (
        <div className="ss-empty">
          所选条件下暂无会话
          {hasFilter && (
            <button className="ss-btn ss-retry" onClick={clearFilter}>
              清除筛选
            </button>
          )}
        </div>
      ) : (
        <>
          <table className="ss-table">
            <thead>
              <tr>
                <th>序号</th>
                {/* 列级固定口径放表头悬浮（help 类＝虚线下划线可悬停暗示）；
                    表头贴顶，气泡向上弹避免盖住第一行数据；
                    行级数值提示留在单元格，避免双提示互相遮挡 */}
                <Tip
                  placement="top"
                  content="聚合器最后已知状态（每 10 秒更新）· 已结束＝进程退出或空闲超 2 小时，与岛面板同口径"
                >
                  <th className="help">状态</th>
                </Tip>
                <th>会话</th>
                <Tip placement="top" content="徽标缩写：CC＝Claude Code · ZC＝ZCode，底色即身份色（设置页可自定义）">
                  <th className="help">Agent</th>
                </Tip>
                <th>模型</th>
                <th>项目</th>
                {/* 可排序四列（点击切换排序键，▼ 指示）与不可排序的出错列交错，
                    顺序必须与下方数据行单元格一一对应：最近｜次数｜出错｜时长｜Token */}
                {sortTh("recent")}
                {sortTh("calls")}
                <Tip placement="top" content="出错的调用条数；悬浮单元格可看具体错误类型">
                  <th className="help">出错</th>
                </Tip>
                <Tip
                  placement="top"
                  content="模型生成时长合计 ·「—」表示该 Agent 的转录文件无时长字段（如 Claude Code）"
                >
                  <th className="help">时长</th>
                </Tip>
                {sortTh("tokens")}
              </tr>
            </thead>
            <tbody>
              {rows.map((r, idx) => {
                const st = displayState(toSessionState(r.state), r.last_ts);
                // 行级派生指标（M1-11）：平均单次 token / 思考占比（与抽屉统计条共用口径）
                const { avgPerCall, thinkPct } = deriveRowMetrics(r);
                return (
                  <tr
                    key={r.session_id}
                    className={`clickable${st.ended ? " ended" : ""}${
                      detail?.row.session_id === r.session_id ? " ss-row-active" : ""
                    }`}
                    onClick={() => openDetail(r)}
                  >
                    {/* 序号：当前筛选结果内连续自增（跨页累计，offset + 行内序位） */}
                    <td className="num ss-seq">{offset + idx + 1}</td>
                    <td>
                      {/* 状态口径说明在表头（列级固定），格内不再叠悬浮提示 */}
                      <span className={`ss-state${r.state === "error" ? " ss-state-err" : ""}`}>
                        <span className={`dot ${st.dot}`} />
                        {r.state === "error" ? errorReason(r.error_types) : st.label}
                      </span>
                    </td>
                    <td title={cardTitle(r.title, r.project_dir, r.session_id)}>
                      {cardTitle(r.title, r.project_dir, r.session_id)}
                    </td>
                    <td>
                      {/* 缩写含义在表头（固定闭合集），徽标自带身份色 */}
                      <span className="ss-badge" style={{ background: agentColor(r.agent) }}>
                        {AGENT_BADGE[r.agent] ?? "??"}
                      </span>
                    </td>
                    <td title={r.model ?? "尚未捕获该会话的模型调用"}>
                      {r.model ?? "--"}
                    </td>
                    <td title={r.project_dir ?? "无法识别工作目录"}>
                      {r.project_dir ? tail(r.project_dir) : "--"}
                    </td>
                    <td className="num">
                      <Tip content={`${fmtDTFull(r.last_ts)}（${fmtRelative(r.last_ts)}）`}>
                        <span>{fmtDT(r.last_ts)}</span>
                      </Tip>
                    </td>
                    <td className="num">
                      <Tip content={`平均 ${fmtTokens(avgPerCall)} token/次`}>
                        <span>{r.calls}</span>
                      </Tip>
                    </td>
                    <td className="num">
                      <Tip
                        content={
                          r.errors > 0
                            ? `错误类型：${r.error_types ?? "未知"}${
                                r.calls > 0
                                  ? ` · 错误率 ${Math.round((r.errors / r.calls) * 100)}%`
                                  : ""
                              }`
                            : "从未出错"
                        }
                      >
                        <span className={r.errors > 0 ? "ss-err" : "ss-err0"}>{r.errors}</span>
                      </Tip>
                    </td>
                    <td className="num">
                      {r.ttft_avg_ms != null ? (
                        <Tip content={`平均首字 ${fmtMs(r.ttft_avg_ms)}`}>
                          <span>{fmtDuration(r.duration_ms)}</span>
                        </Tip>
                      ) : (
                        fmtDuration(r.duration_ms)
                      )}
                    </td>
                    <td className="num">
                      <Tip
                        content={
                          <div className="tip-breakdown">
                            <div>
                              输入 {fmtTokens(r.input_tokens)} · 输出 {fmtTokens(r.output_tokens)}
                            </div>
                            <div>
                              缓存读 {fmtTokens(r.cache_read_tokens)} · 缓存写{" "}
                              {fmtTokens(r.cache_creation_tokens)}
                            </div>
                            <div>
                              思考 {fmtTokens(r.reasoning_tokens)}
                              {thinkPct != null && `（占比 ${thinkPct}）`}
                            </div>
                            <div>
                              平均 {fmtTokens(avgPerCall)}/次 · 首末相隔{" "}
                              {fmtDuration(Math.max(0, r.last_ts - r.first_ts))}
                            </div>
                            <div className="tip-dim">
                              会话累计 · 含缓存，与账单同口径；思考占比不含缓存；首末相隔为墙钟跨度
                            </div>
                          </div>
                        }
                      >
                        <span>{fmtTokens(r.total_tokens)}</span>
                      </Tip>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>

          {/* 分页器：每页行数选择 + 翻页 */}
          <div className="ss-pager">
            <span className="ss-page-no">
              共 {page?.total ?? 0} 个会话 · 第 {pageNo} / {pageCount} 页
            </span>
            <label className="ss-pagesize">
              每页
              <select
                value={pageSize}
                onChange={(e) => setPageSize(Number(e.target.value))}
              >
                {PAGE_SIZES.map((n) => (
                  <option key={n} value={n}>
                    {n}
                  </option>
                ))}
              </select>
              行
            </label>
            <button className="ss-btn" disabled={pageNo <= 1} onClick={() => turnPage(-1)}>
              上一页
            </button>
            <button className="ss-btn" disabled={pageNo >= pageCount} onClick={() => turnPage(1)}>
              下一页
            </button>
          </div>
        </>
      )}

      {/* 会话详情抽屉：头区一行＋统计条＋页签（调用流水/状态时间线）＋正序倒序；遮罩点击/Esc 关闭 */}
      {detail && (
        <>
          <div className="ss-mask" onClick={() => setDetail(null)} />
          <aside className="ss-drawer">
            {/* 关闭按钮：绝对定位悬浮在抽屉左上角外侧（窄窗口媒体查询回贴抽屉内） */}
            <button className="ss-drawer-close" title="关闭（Esc）" onClick={() => setDetail(null)}>
              ×
            </button>
            {/* 头区一行：状态点＋标题＋跳转（关闭已外置，标题空间更宽裕） */}
            <div className="ss-drawer-head">
              <div className="ss-drawer-title">
                <span className={`dot ${displayState(toSessionState(detail.row.state), detail.row.last_ts).dot}`} />
                <b title={cardTitle(detail.row.title, detail.row.project_dir, detail.row.session_id)}>
                  {cardTitle(detail.row.title, detail.row.project_dir, detail.row.session_id)}
                </b>
              </div>
              <div className="ss-drawer-headtools">
                {/* 跳转仅近 30 分钟活跃会话渲染：不活跃时隐藏而非置灰——
                    原生 disabled 不派发鼠标事件会让说明悬浮失效，「出现即可用」零猜测 */}
                {Date.now() - detail.row.last_ts <= JUMPABLE_MS && (
                  <Tip content="激活该会话对应的终端/IDE 窗口">
                    <button className="ss-btn ss-drawer-jump" onClick={() => jump(detail.row)}>
                      跳转
                    </button>
                  </Tip>
                )}
              </div>
            </div>
            {/* 元信息：Agent 全名徽标＋模型＋最后活动相对时间（抽屉是全量信息视图，不做缩写截断） */}
            <div className="ss-drawer-meta">
              <span
                className="ss-badge"
                style={{ background: agentColor(detail.row.agent) }}
                title={AGENT_DEFS.find((a) => a.id === detail.row.agent)?.label ?? detail.row.agent}
              >
                {AGENT_DEFS.find((a) => a.id === detail.row.agent)?.label ?? detail.row.agent}
              </span>
              <span className="ss-drawer-model" title={detail.row.model ?? "尚未捕获该会话的模型调用"}>
                {detail.row.model ?? "--"}
              </span>
              <Tip content={`首次 ${fmtDTFull(detail.row.first_ts)} · 最后 ${fmtDTFull(detail.row.last_ts)}`}>
                <span className="ss-drawer-rel">最后活动 {fmtRelative(detail.row.last_ts)}</span>
              </Tip>
            </div>
            <div className="ss-drawer-path" title={detail.row.project_dir ?? "无法识别工作目录"}>
              {detail.row.project_dir ?? "无法识别工作目录"}
            </div>
            {/* 统计条：五格汇总＋弱色明细（全部来自行数据零额外查询，口径与主表格 Tip 同源） */}
            {dr && drMetrics && (
              <>
                <div className="ss-drawer-stats">
                  <div className="ss-drawer-stat">
                    <span className="ss-drawer-stat-label">调用</span>
                    <span className="ss-drawer-stat-value">
                      {dr.calls}
                      {dr.errors > 0 && <span className="ss-drawer-stat-err">错 {dr.errors}</span>}
                    </span>
                  </div>
                  <div className="ss-drawer-stat">
                    <span className="ss-drawer-stat-label">总 Token</span>
                    <Tip content="会话累计·含缓存，与账单同口径">
                      <span className="ss-drawer-stat-value">{fmtTokens(dr.total_tokens)}</span>
                    </Tip>
                  </div>
                  <div className="ss-drawer-stat">
                    <span className="ss-drawer-stat-label">输出</span>
                    <span className="ss-drawer-stat-value">{fmtTokens(dr.output_tokens)}</span>
                  </div>
                  <div className="ss-drawer-stat">
                    <span className="ss-drawer-stat-label">时长</span>
                    <Tip content={dr.duration_ms != null ? "模型生成时长合计" : "该 Agent 的转录文件无时长字段"}>
                      <span className="ss-drawer-stat-value">
                        {dr.duration_ms != null ? fmtDuration(dr.duration_ms) : "—"}
                      </span>
                    </Tip>
                  </div>
                  <div className="ss-drawer-stat">
                    <span className="ss-drawer-stat-label">平均/次</span>
                    <span className="ss-drawer-stat-value">{fmtTokens(drMetrics.avgPerCall)}</span>
                  </div>
                </div>
                <div className="ss-drawer-stat-detail">
                  输入 {fmtTokens(dr.input_tokens)} · 缓存读 {fmtTokens(dr.cache_read_tokens)} · 缓存写{" "}
                  {fmtTokens(dr.cache_creation_tokens)} · 思考 {fmtTokens(dr.reasoning_tokens)}
                  {drMetrics.thinkPct != null && `（占比 ${drMetrics.thinkPct}）`}
                  {dr.ttft_avg_ms != null && ` · 平均首字 ${fmtMs(dr.ttft_avg_ms)}`}
                  {dr.errors > 0 && dr.calls > 0 && ` · 错误率 ${Math.round((dr.errors / dr.calls) * 100)}%`}
                </div>
              </>
            )}
            {detail.error ? (
              <div className="ss-drawer-err">加载失败：{detail.error}</div>
            ) : detail.data == null ? (
              <div className="ss-drawer-loading">加载中…</div>
            ) : (
              <>
                {/* 页签栏：两页签＋右侧排序切换/刷新 */}
                <div className="ss-drawer-tabs">
                  <button
                    className={`ss-drawer-tab${detailTab === "calls" ? " on" : ""}`}
                    onClick={() => setDetailTab("calls")}
                  >
                    调用流水 {callsCountText}
                  </button>
                  <button
                    className={`ss-drawer-tab${detailTab === "events" ? " on" : ""}`}
                    onClick={() => setDetailTab("events")}
                  >
                    状态时间线 {eventsCountText}
                  </button>
                  <div className="ss-drawer-tools">
                    <Tip
                      content={
                        detailOrder === "desc"
                          ? "当前为降序：最新在上。点击切换为升序（最早在上），调用与时间线同时生效"
                          : "当前为升序：最早在上。点击切换为降序（最新在上），调用与时间线同时生效"
                      }
                    >
                      <button
                        className="ss-drawer-order"
                        onClick={() => setDetailOrder(detailOrder === "desc" ? "asc" : "desc")}
                      >
                        {detailOrder === "desc" ? "降序" : "升序"}
                        {/* 正/倒三角标识顺序：与主表格排序标记同一符号系统 */}
                        <span className="ss-drawer-order-mark">
                          {detailOrder === "desc" ? " ▼" : " ▲"}
                        </span>
                      </button>
                    </Tip>
                    <button className="ss-drawer-refresh" title="重新加载流水与时间线" onClick={refreshDetail}>
                      ⟳ 刷新
                    </button>
                  </div>
                </div>
                {/* 页签内容：唯一滚动区，调用/时间线各自独占 */}
                <div className="ss-drawer-body">
                  {detailTab === "calls" ? (
                    <div>
                      {detail.data.calls.length === 0 ? (
                        <div className="ss-drawer-empty">
                          无调用记录（该会话未捕获到模型调用：转录缺失或尚未产生调用）
                        </div>
                      ) : (
                        <>
                          {detail.data.calls_total > detail.data.calls.length && (
                            <div className="ss-trunc">
                              仅加载最近 {detail.data.calls.length} 条，更早的调用未显示
                            </div>
                          )}
                          {withDaySeps(detailCalls).map((cell, i) => {
                            // 局部常量收窄可选属性：复制按钮回调内安全引用
                            const c = cell.item;
                            if (cell.sep) {
                              return (
                                <div key={`sep-${i}`} className="ss-day-sep">
                                  <span>{cell.sep}</span>
                                </div>
                              );
                            }
                            if (!c) return null;
                            return (
                              <div
                                key={`c-${c.ts}-${i}`}
                                className={`ss-call${c.error_type ? " ss-call-err" : ""}`}
                              >
                                <div className="ss-call-line1">
                                  <span className="ss-call-time">{fmtHMS(c.ts)}</span>
                                  <span className="ss-call-model">{c.model ?? "--"}</span>
                                  {/* 合计 token（含缓存，与账单同口径）右对齐加粗，扫表主数字 */}
                                  <span className="ss-call-total">
                                    {fmtTokens(
                                      c.input_tokens +
                                        c.output_tokens +
                                        c.reasoning_tokens +
                                        c.cache_read_tokens +
                                        c.cache_creation_tokens,
                                    )}
                                  </span>
                                  {c.error_type && (
                                    <Tip content={c.error_type}>
                                      <span className="ss-call-errtag">{errorReason(c.error_type)}</span>
                                    </Tip>
                                  )}
                                </div>
                                <div className="ss-call-line2">
                                  <span>输入 {fmtTokens(c.input_tokens)}</span>
                                  <span>输出 {fmtTokens(c.output_tokens)}</span>
                                  <span>思考 {fmtTokens(c.reasoning_tokens)}</span>
                                  <span>缓存读 {fmtTokens(c.cache_read_tokens)}</span>
                                  <span>缓存写 {fmtTokens(c.cache_creation_tokens)}</span>
                                  {c.duration_ms != null && (
                                    <span>时长 {fmtDuration(c.duration_ms)}</span>
                                  )}
                                  {c.ttft_ms != null && <span>首字延迟 {fmtMs(c.ttft_ms)}</span>}
                                  {/* 复制本条明细（精确数值键值对）：悬浮卡片时出现，贴给 AI/issue 排查用 */}
                                  <button
                                    className={`ss-copy${copiedKey === `c-${c.ts}-${i}` ? " ok" : ""}`}
                                    title="复制本条调用明细（精确数值）"
                                    onClick={() => copyEntry(`c-${c.ts}-${i}`, buildCallCopyText(c))}
                                  >
                                    {copiedKey === `c-${c.ts}-${i}` ? "已复制" : "复制"}
                                  </button>
                                </div>
                              </div>
                            );
                          })}
                        </>
                      )}
                    </div>
                  ) : (
                    <div>
                      {detail.data.events.length === 0 ? (
                        <div className="ss-drawer-empty">无状态事件记录（如 hooks 未启用）</div>
                      ) : (
                        <>
                          {detail.data.events_total > detail.data.events.length && (
                            <div className="ss-trunc">
                              仅加载最近 {detail.data.events.length} 条，更早的事件未显示
                            </div>
                          )}
                          {withDaySeps(detailEvents).map((cell, i) => {
                            // 局部常量收窄可选属性：复制按钮回调内安全引用
                            const ev = cell.item;
                            if (cell.sep) {
                              return (
                                <div key={`esep-${i}`} className="ss-day-sep">
                                  <span>{cell.sep}</span>
                                </div>
                              );
                            }
                            if (!ev) return null;
                            return (
                              <div key={`e-${ev.ts}-${i}`} className="ss-event">
                                <span className="ss-event-time">{fmtHMS(ev.ts)}</span>
                                <Tip content={ev.payload ?? ""}>
                                  <span className="ss-event-hook">{ev.hook ?? "未知事件"}</span>
                                </Tip>
                                {/* 复制事件原文（时间＋hook＋payload）：悬浮时出现 */}
                                <button
                                  className={`ss-copy${copiedKey === `e-${ev.ts}-${i}` ? " ok" : ""}`}
                                  title="复制本条事件原文"
                                  onClick={() => copyEntry(`e-${ev.ts}-${i}`, buildEventCopyText(ev))}
                                >
                                  {copiedKey === `e-${ev.ts}-${i}` ? "已复制" : "复制"}
                                </button>
                              </div>
                            );
                          })}
                        </>
                      )}
                    </div>
                  )}
                </div>
              </>
            )}
          </aside>
        </>
      )}

      <div className="ss-foot">
        状态口径与岛面板一致（空闲超 2 小时按已结束展示）· 点击行查看调用流水与状态时间线，抽屉内可跳转近
        30 分钟活跃会话 · 生成时长仅 ZCode 等转录含时长字段的 Agent · 数据源为本地 SQLite，每次打开/刷新时查询
      </div>
    </div>
  );
}
