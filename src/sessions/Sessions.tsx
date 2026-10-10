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
import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { save } from "@tauri-apps/plugin-dialog";
import SearchSelect from "../shared/SearchSelect";
import { useFocusTrap } from "../shared/useFocusTrap";
import EmptyState from "../shared/EmptyState";
import Toast, { type ToastData } from "../shared/Toast";
import { ArrowIcon, CloseIcon, RefreshIcon, SessionsIcon, WarnIcon } from "../shared/icons";
import {
  cardTitle,
  displayState,
  agentFullName,
  agentShortName,
  endedAfterHours,
  isJumpable,
  toSessionState,
  syncDisplayConstants,
} from "../shared/sessionDisplay";
import { fmtDT, fmtDTFull, fmtHMS, daySepLabel, fmtMs, tail } from "../shared/format";
import {
  errorReason,
  fmtDuration,
  fmtRelative,
  fmtTokens,
} from "../shared/types";
import AgentBadge from "../shared/AgentBadge";
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

/** 状态档（与 Rust session_page 白名单同步）。空闲阈值取 display_constants
 *  下发值（endedAfterHours，2026-10-10 审查修复：原硬编码「2 小时」与后端
 *  常量脱钩，后端调常量文案即失真）——故为函数而非模块常量，渲染时取当前值 */
const statusFilters = (): { key: string; label: string; tip: string }[] => [
  { key: "all", label: "全部", tip: "不过滤状态" },
  { key: "active", label: "进行中", tip: `工作中/等待输入/出错，或 ${endedAfterHours()} 小时内仍空闲` },
  { key: "ended", label: "已结束", tip: `进程已退出，或空闲超过 ${endedAfterHours()} 小时` },
  { key: "errored", label: "有错误", tip: "范围内出现过出错的调用" },
];

/** 排序键（与 Rust session_page 白名单同步；方向固定降序，升序对管理场景无意义） */
const SORTS: Record<string, string> = {
  recent: "最近",
  tokens: "Token",
  calls: "次数",
  duration: "时长",
};

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
  useTheme({ syncNative: true }); // 深浅主题跟随；syncNative＝标题栏颜色随应用主题（07-UX 2.4）
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
  // 跳页输入草稿（07-UX 3.2）：受控值，Enter 提交或失焦即清空——占位符恒显
  // 当前页码，不留陈旧草稿误导
  const [jumpDraft, setJumpDraft] = useState("");
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [stamp, setStamp] = useState("");
  // 导出反馈 toast（2026-10-09 toast 化，与报表页/设置页同一范式）：成功带
  // 「打开所在目录」动作 6s 自动消失（悬停暂停），失败常驻可关闭——旧内联
  // tip 芯片会挤压头部按钮，连根移除
  const [toast, setToast] = useState<ToastData | null>(null);
  // 导出进行中（五轮审查防重入，同报表页）：保存对话框挂起时连点会叠开第二个对话框
  const [exporting, setExporting] = useState(false);
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

  // 展示常量同步（#20）：「已结束」阈值从后端拉取（Rust 单真值源），失败保持默认
  useEffect(() => {
    void syncDisplayConstants();
  }, []);

  // M2-UX-1：岛面板历史区筛选中点「查看更多会话」→ 带 Agent 预筛选跳转。
  // 窗口常驻隐藏（关闭即 hide 不销毁），监听常挂；空负载视作清除筛选
  useEffect(() => {
    const un = listen<string | null>("sessions-prefilter", (e) =>
      setAgent(e.payload || null),
    );
    return () => {
      un.then((f) => f());
    };
  }, []);

  // 页请求令牌（四轮审查）：session_page 是 async 命令（线程池执行），连续切
  // 筛选/防抖击键会并发多个请求，后发先至时旧响应会覆盖新数据且无自愈——
  // 落定时只认最新令牌（与同文件 fetchDetail 的过期守卫同哲学）
  const pageSeq = useRef(0);
  // 是否已有过成功数据（失败反馈分流：有旧数据→保留表格＋错误 toast；
  // 无（首载失败）→错误空态。ref 免 useCallback 闭包过期问题）
  const hasDataRef = useRef(false);

  /** 拉取一页（o=偏移）；筛选变更拉第一页，翻页拉目标页 */
  const fetchPage = useCallback(
    async (o: number) => {
      const seq = ++pageSeq.current;
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
        if (seq !== pageSeq.current) return; // 已有更新请求在途/落定，丢弃过期响应
        setPage(p);
        setError(null);
        hasDataRef.current = true;
        setStamp(fmtDTFull(Date.now()));
      } catch (e) {
        if (seq !== pageSeq.current) return;
        setError(String(e));
        // 失败反馈收口（2026-10-10 审查，对齐报表页「失败≠换页」纪律）：手上
        // 有旧数据时保留旧表格＋错误 toast 报告失败（原先进 EmptyState 整表被
        // 替换、滚动位置/页码上下文全丢）；首载失败（无旧数据）不重复弹 toast，
        // 走渲染层错误空态
        if (hasDataRef.current) {
          setToast({ text: `刷新失败：${String(e)}`, kind: "error" });
        }
      } finally {
        if (seq === pageSeq.current) setLoaded(true);
      }
    },
    [range, agent, project, model, status, keyword, sort, pageSize],
  );

  // 筛选/搜索/排序变更：回到第一页重拉；下拉选项只随范围变化（四轮审查拆出：
  // 原先挂在同一 effect 里，防抖击键/切排序也会多触发一次 session_options）
  useEffect(() => {
    setLoaded(false);
    setOffset(0);
    void fetchPage(0);
  }, [range, status, agent, project, model, keyword, sort, fetchPage]);

  useEffect(() => {
    invoke<FilterOptions>("session_options", { range })
      .then(setOptions)
      .catch(() => {}); // 选项拉取失败不打扰：下拉 retains 上一次内容
  }, [range]);

  /** 翻页：只重拉会话表；翻页后视口回顶（2026-10-03 审查补充：末页翻回首页时
   *  视口仍停在页尾空白，看起来像空页） */
  const turnPage = (dir: 1 | -1) => {
    const size = page?.page_size ?? 20;
    const total = page?.total ?? 0;
    const pages = Math.max(1, Math.ceil(total / size));
    const next = Math.min(Math.max(offset + dir * size, 0), (pages - 1) * size);
    if (next === offset) return;
    setOffset(next);
    void fetchPage(next).then(() => {
      document.querySelector(".ss-root")?.scrollTo({ top: 0 });
    });
  };

  /** 手动刷新：原地重拉当前页（数据时点诚实呈现于「数据截至」） */
  const refresh = () => void fetchPage(offset);

  /** 跳页（07-UX 3.2）：输入页码 Enter 跳转——空值/非数字不动作，越界钳回
   *  [1, 总页数]；与翻页同款拉取＋视口回顶。无论是否成行都清空草稿 */
  const jumpToPage = (raw: string) => {
    setJumpDraft("");
    const n = Number(raw.trim());
    if (!raw.trim() || !Number.isFinite(n)) return;
    const size = page?.page_size ?? 20;
    const pages = Math.max(1, Math.ceil((page?.total ?? 0) / size));
    const next = (Math.min(Math.max(Math.round(n), 1), pages) - 1) * size;
    if (next === offset) return;
    setOffset(next);
    void fetchPage(next).then(() => {
      document.querySelector(".ss-root")?.scrollTo({ top: 0 });
    });
  };

  /** 导出当前筛选＋排序的全量会话 CSV：先弹系统保存对话框（取消则不动作），
   *  成功后提示文字可点击定位文件 */
  const doExport = async () => {
    if (exporting) return; // 防重入（五轮审查，同报表页）
    setExporting(true);
    try {
      const d = new Date();
      const p2 = (n: number) => String(n).padStart(2, "0");
      const stampText = `${d.getFullYear()}${p2(d.getMonth() + 1)}${p2(d.getDate())}-${p2(d.getHours())}${p2(d.getMinutes())}${p2(d.getSeconds())}`;
      const target = await save({
        title: "导出会话列表 CSV",
        defaultPath: `去你的岛-会话列表-${stampText}.csv`,
        filters: [{ name: "CSV 文件", extensions: ["csv"] }],
      });
      if (!target) return; // 用户取消
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
        // 成功 toast（与报表页同款）：完整文件名放得下（浮层不占头部布局）；
        // 「打开所在目录」为显式点击（不自动弹资源管理器——四轮审查拍板保留）
        setToast({
          text: `已导出：${target.split(/[\\/]/).pop()}`,
          kind: "ok",
          action: {
            label: "打开所在目录",
            onClick: () =>
              invoke("open_file_location", { path: target }).catch((e) =>
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
    if (!isJumpable(r.last_ts)) return;
    invoke("focus_session", { sessionId: r.session_id }).catch(() => {});
  };

  // Esc 关闭详情抽屉；焦点陷阱（useFocusTrap：开聚焦/循环/关归焦）
  const drawerRef = useRef<HTMLElement>(null);
  useFocusTrap(drawerRef, detail != null);
  useEffect(() => {
    if (!detail) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setDetail(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [detail]);

  /** 可排序表头单元格：点击切换排序键，当前键带 ▼ 指示（顺序须与数据行一致）。
   *  键盘可达（四轮审查）：原先纯 onClick 的 th 不可聚焦，排序功能键盘不可达
   *  ——对齐报表维度条（rp-dim-row）的 role+tabIndex+Enter 先例 */
  const sortTh = (k: keyof typeof SORTS) => (
    <th
      key={k}
      className="sortable"
      role="button"
      tabIndex={0}
      aria-sort={sort === k ? "descending" : undefined}
      onClick={() => setSort(k)}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          setSort(k);
        }
      }}
    >
      {SORTS[k]}
      {sort === k && <span className="ss-sort-mark"> <ArrowIcon dir="down" /></span>}
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
      {/* 导出反馈 toast（fixed 贴窗顶居中，不占头部布局——挤压问题根治于此） */}
      <Toast data={toast} onDismiss={() => setToast(null)} />
      {/* 页头：标题 + 口径说明 + 数据截至 + 刷新 + 导出 */}
      <div className="ss-header">
        <div className="ss-title-wrap">
          <span className="ss-title">会话</span>
          {/* 多行提示必须用表达式形式：引号属性里的 \n 是字面反斜杠＋n 不换行
              （同 Quotas「推算值」title 的 07-UX 3.9 修复，2026-10-10 全局清残余） */}
          <Tip content={"本窗口仅统计产生过模型调用的会话\n岛面板按转录/源库文件扫描，包含打开过但未产生调用的会话，因此两处数量可能不同"}>
            <span className="ss-title-note">仅统计产生过调用的会话</span>
          </Tip>
        </div>
        <div className="ss-header-right">
          {stamp && (
            <Tip content="打开/刷新时查询本地数据库的时间点（不做自动轮询）">
              <span className="ss-stamp">数据截至 {stamp.slice(5)}</span>
            </Tip>
          )}
          <button className="ss-btn" onClick={refresh}>
            刷新
          </button>
          <button className="ss-btn ss-export" onClick={doExport} disabled={exporting}>
            {exporting ? "导出中…" : "导出 CSV"}
          </button>
          {/* 设置入口（07-UX 1.3）：与报表页同款文字按钮（头部动作族统一
              「文字为体」，不做 icon-only 新形态族），不做跨窗口导航条 */}
          <button
            className="ss-btn"
            title="打开设置窗口"
            onClick={() => invoke("show_settings_window").catch(() => {})}
          >
            设置
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
              <CloseIcon />
            </button>
          )}
        </div>
        <div className="ss-chips">
          {statusFilters().map((s) => (
            <Tip key={s.key} content={s.tip}>
              <button
                className={`ss-chip${status === s.key ? " on" : ""}`}
                aria-pressed={status === s.key}
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
          options={(options?.agents ?? []).map((a) => ({
            value: a,
            // 全名口径（2026-10-03 审查修复）：与报表页筛选下拉一致——
            // 此前显示原始 id（claude-code），两窗口同位两种口径
            label: agentFullName(a),
          }))}
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

      {/* 查询失败反馈分流（2026-10-10 审查收口，对齐报表页「失败≠换页」）：
          首载失败（无旧数据）→ 显式错误态＋重试（不弹窗，红线⑤）；有旧数据时
          保留表格，失败由 fetchPage 内 toast 报告。07-UX 2.1 起统一走 EmptyState
          模板（WarnIcon 与真空态区分排障方向） */}
      {error && page == null ? (
        <EmptyState
          icon={<WarnIcon />}
          title="查询失败"
          desc={error}
          action={{ label: "重试", onClick: refresh }}
        />
      ) : !loaded ? (
        /* 加载中：轻量一行灰字（07-UX 2.1，不上真空态模板） */
        <div className="ss-empty">加载中…</div>
      ) : rows.length === 0 ? (
        hasFilter ? (
          /* 筛选空态：用户任务中途，保持轻量文案＋清除筛选（07-UX 2.1 明确保留） */
          <div className="ss-empty">
            所选条件下暂无会话
            <button className="ss-btn ss-retry" onClick={clearFilter}>
              清除筛选
            </button>
          </div>
        ) : (
          /* 真空态（07-UX 2.1 统一模板）：从未有过会话，无主动作可给 */
          <EmptyState
            icon={<SessionsIcon />}
            title="还没有会话记录"
            desc="开始使用任意 Agent 后，会话会自动出现在这里"
          />
        )
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
                  content={`聚合器最后已知状态（每 10 秒更新）\n已结束＝进程退出或空闲超 ${endedAfterHours()} 小时，与岛面板同口径`}
                >
                  <th className="help">状态</th>
                </Tip>
                <th>会话</th>
                <Tip placement="top" content="徽章为身份色胶囊包裹 Agent 短名（悬浮出全名），身份色可在设置页自定义">
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
                  content={"模型生成时长合计\n「—」表示该 Agent 的转录文件无时长字段（如 Claude Code）"}
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
                    /* 键盘可达（2026-10-03 审查补充）：行可聚焦、Enter 开抽屉——
                       对齐设置页 st-agent 卡的 role/tabIndex/Enter 先例 */
                    role="button"
                    tabIndex={0}
                    onClick={() => openDetail(r)}
                    onKeyDown={(e) => {
                      // Enter＋Space 双键（2026-10-10 审查 a11y 对齐 WAI-APG
                      // button 模式，与 sortTh/DimBars 同款）
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        openDetail(r);
                      }
                    }}
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
                      {/* Agent 徽章（2026-09-30 徽章统一改版）：身份色胶囊包裹短名
                          （悬浮出全名），与岛面板/设置页同一枚 shared/AgentBadge */}
                      <AgentBadge
                        agent={r.agent}
                        name={agentShortName(r.agent)}
                        title={agentFullName(r.agent)}
                      />
                    </td>
                    <td title={r.model ?? "尚未捕获该会话的模型调用"}>
                      {r.model ?? "--"}
                    </td>
                    <td title={r.project_dir ?? "无法识别工作目录"}>
                      {r.project_dir ? tail(r.project_dir) : "--"}
                    </td>
                    <td className="num">
                      {/* 表格内 Tip 一律 focusable={false}（2026-10-10 审查 a11y）：
                          默认注入 tabIndex 会把 Tab 序污染到不可用（每行数个 Tip
                          ×20~100 行）；键盘聚焦行本身即可，悬浮通道保留——
                          同年度热力图 365 格先例 */}
                      <Tip focusable={false} content={`${fmtDTFull(r.last_ts)}（${fmtRelative(r.last_ts)}）`}>
                        <span>{fmtDT(r.last_ts)}</span>
                      </Tip>
                    </td>
                    <td className="num">
                      <Tip focusable={false} content={`平均 ${fmtTokens(avgPerCall)} token/次`}>
                        <span>{r.calls}</span>
                      </Tip>
                    </td>
                    <td className="num">
                      <Tip
                        focusable={false}
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
                        <Tip focusable={false} content={`模型生成时长合计 · 平均首字 ${fmtMs(r.ttft_avg_ms)}`}>
                          <span>{fmtDuration(r.duration_ms)}</span>
                        </Tip>
                      ) : (
                        fmtDuration(r.duration_ms)
                      )}
                    </td>
                    <td className="num">
                      <Tip
                        focusable={false}
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

          {/* 分页器：每页行数选择 + 翻页 + 跳页（07-UX 3.2 窄输入框 Enter 跳转） */}
          <div className="ss-pager">
            <span className="ss-page-no">
              共 {page?.total ?? 0} 个会话 · 第 {pageNo} / {pageCount} 页
            </span>
            <input
              className="ss-jump"
              value={jumpDraft}
              onChange={(e) => setJumpDraft(e.target.value.replace(/\D/g, ""))}
              placeholder="跳页"
              inputMode="numeric"
              aria-label="输入页码，按 Enter 跳转"
              title={"输入页码后按 Enter 跳转\nEsc 清空"}
              onKeyDown={(e) => {
                if (e.key === "Enter") jumpToPage(jumpDraft);
                if (e.key === "Escape") {
                  setJumpDraft("");
                  (e.target as HTMLInputElement).blur();
                }
              }}
              onBlur={() => setJumpDraft("")}
            />
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

      {/* 会话详情抽屉：头区一行＋统计条＋页签（调用流水/状态时间线）＋正序倒序；
          遮罩点击/Esc 关闭；焦点陷阱（2026-10-03 审查新增：打开初始聚焦/Tab
          首尾循环/关闭归焦，抽屉语义补齐 role=dialog） */}
      {detail && (
        <>
          <div className="ss-mask" onClick={() => setDetail(null)} />
          <aside
            className="ss-drawer"
            role="dialog"
            aria-modal="true"
            aria-label="会话详情"
            ref={drawerRef}
          >
            {/* 关闭按钮：绝对定位悬浮在抽屉左上角外侧（窄窗口媒体查询回贴抽屉内） */}
            <button className="ss-drawer-close" title="关闭（Esc）" onClick={() => setDetail(null)}>
              <CloseIcon />
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
                {isJumpable(detail.row.last_ts) && (
                  <Tip content={"激活该会话对应的终端/IDE 窗口\n仅近 30 分钟内有活动的会话可跳转；抽屉停留较久后按钮仍显示但点击不再响应（判定按点击时刻计算）\n关闭重开抽屉即按新时刻重新判定"}>
                    <button className="ss-btn ss-drawer-jump" onClick={() => jump(detail.row)}>
                      跳转
                    </button>
                  </Tip>
                )}
              </div>
            </div>
            {/* 元信息：Agent 全名徽章＋模型＋最后活动相对时间（抽屉是全量信息视图，不做缩写截断） */}
            <div className="ss-drawer-meta">
              <AgentBadge
                agent={detail.row.agent}
                name={agentFullName(detail.row.agent)}
                size="lg"
              />
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
                    {/* 统计值截断兜底（C2）：窄窗下统计条五格会 ellipsis——
                        无既有提示的格补原生 title（标签＋完整值） */}
                    <span className="ss-drawer-stat-value" title={`调用 ${dr.calls} 次`}>
                      {dr.calls}
                      {dr.errors > 0 && <span className="ss-drawer-stat-err">错 {dr.errors}</span>}
                    </span>
                  </div>
                  <div className="ss-drawer-stat">
                    <span className="ss-drawer-stat-label">总 Token</span>
                    {/* 数值行＋弱色口径行（屏显为 K/M 缩写，精确千分位是真实增量） */}
                    <Tip
                      content={
                        <div className="tip-breakdown">
                          <div>
                            共 {fmtTokens(dr.total_tokens)}（{dr.total_tokens.toLocaleString()}）
                          </div>
                          <div className="tip-dim">会话累计 · 含缓存，与账单同口径</div>
                        </div>
                      }
                    >
                      <span className="ss-drawer-stat-value">{fmtTokens(dr.total_tokens)}</span>
                    </Tip>
                  </div>
                  <div className="ss-drawer-stat">
                    <span className="ss-drawer-stat-label">输出</span>
                    <span
                      className="ss-drawer-stat-value"
                      title={`输出 ${dr.output_tokens.toLocaleString()} token`}
                    >
                      {fmtTokens(dr.output_tokens)}
                    </span>
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
                    <span
                      className="ss-drawer-stat-value"
                      title={`平均 ${fmtTokens(drMetrics.avgPerCall)} token/次`}
                    >
                      {fmtTokens(drMetrics.avgPerCall)}
                    </span>
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
                {/* 页签栏：两页签＋右侧排序切换/刷新；页签悬浮说明（2026-10-10 方案 A）：
                    名字保留检索稳定性，理解成本由悬浮消化——与表头列说明同一套语言 */}
                <div className="ss-drawer-tabs">
                  <Tip placement="bottom" content={"每次模型调用的 token 与耗时明细"}>
                    <button
                      className={`ss-drawer-tab${detailTab === "calls" ? " on" : ""}`}
                      onClick={() => setDetailTab("calls")}
                    >
                      调用流水 {callsCountText}
                    </button>
                  </Tip>
                  <Tip
                    placement="bottom"
                    content={"钩子事件按序记录（提交提示、工具调用前后等），可还原状态变化"}
                  >
                    <button
                      className={`ss-drawer-tab${detailTab === "events" ? " on" : ""}`}
                      onClick={() => setDetailTab("events")}
                    >
                      状态时间线 {eventsCountText}
                    </button>
                  </Tip>
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
                          {detailOrder === "desc" ? <> <ArrowIcon dir="down" /></> : <> <ArrowIcon dir="up" /></>}
                        </span>
                      </button>
                    </Tip>
                    <button className="ss-drawer-refresh" title="重新加载流水与时间线" onClick={refreshDetail}>
                      <RefreshIcon /> 刷新
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
                                  {/* 模型名截断兜底（C3）：长模型名 ellipsis 后无提示不可读 */}
                                  <span className="ss-call-model" title={c.model ?? undefined}>
                                    {c.model ?? "--"}
                                  </span>
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

      {/* 页脚分条短句（2026-10-10 对齐报表页脚 07-UX 3.3 同款拆分）：原 100 余字
          单行塞四个口径（状态/交互指引/时长覆盖/数据源）难扫读，按语义分行 */}
      <div className="ss-foot">
        <div>状态口径与岛面板一致：空闲超 {endedAfterHours()} 小时按已结束展示</div>
        <div>点击行查看调用流水与状态时间线；抽屉内可跳转近 30 分钟活跃会话</div>
        <div>生成时长仅部分 Agent 提供（转录含时长字段，如 ZCode）</div>
        <div>数据源为本地 SQLite，每次打开/刷新时查询</div>
      </div>
    </div>
  );
}
