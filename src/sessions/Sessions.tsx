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
import { fmtDT, fmtDTFull, tail } from "../shared/format";
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
  duration_ms: number | null;
  errors: number;
  error_types: string | null;
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

/** 范围档（与 Rust build_filter 白名单同步；默认近 7 天——首屏更快也更贴最近关注） */
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

export default function Sessions() {
  useTheme(); // 深浅主题跟随（与其他窗口同一套 Hook）
  // 筛选状态：范围（默认近 7 天）＋状态档＋三维度（null=全部）＋关键字（防抖后生效）
  const [range, setRange] = useState("7d");
  const [status, setStatus] = useState("all");
  const [sort, setSort] = useState("recent");
  const [agent, setAgent] = useState<string | null>(null);
  const [project, setProject] = useState<string | null>(null);
  const [model, setModel] = useState<string | null>(null);
  const [search, setSearch] = useState(""); // 输入框实时值
  const [keyword, setKeyword] = useState(""); // 防抖后真正参与查询的词
  // 数据：分页结果 + 下拉选项 + 加载/错误态 + 数据截至时间戳
  const [page, setPage] = useState<SessionPage | null>(null);
  const [options, setOptions] = useState<FilterOptions | null>(null);
  const [offset, setOffset] = useState(0);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [stamp, setStamp] = useState("");
  // CSV 导出提示：text 为展示文案；path 非空时可点击定位到文件
  const [exportTip, setExportTip] = useState<{ text: string; path: string | null } | null>(null);

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
    [range, agent, project, model, status, keyword, sort],
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

  /** 行点击跳转：仅新鲜会话可跳（复用 T10 窗口匹配），未命中静默失败 */
  const jump = (r: SessionRow) => {
    if (Date.now() - r.last_ts > JUMPABLE_MS) return;
    invoke("focus_session", { sessionId: r.session_id }).catch(() => {});
  };

  const rows = page?.rows ?? [];
  const pageSize = page?.page_size ?? 20;
  const pageCount = Math.max(1, Math.ceil((page?.total ?? 0) / pageSize));
  const pageNo = Math.floor(offset / pageSize) + 1;
  const hasFilter = agent != null || project != null || model != null || keyword !== "" || status !== "all";
  const clearFilter = () => {
    setAgent(null);
    setProject(null);
    setModel(null);
    setStatus("all");
    setSearch("");
    setKeyword("");
  };

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
                <th>状态</th>
                <th>会话</th>
                <th>Agent</th>
                <th>模型</th>
                <th>项目</th>
                {/* 可排序四列：点击切换排序键（方向固定降序，▼ 指示） */}
                {(["recent", "calls", "duration", "tokens"] as const).map((k) => (
                  <th key={k} className="sortable" onClick={() => setSort(k)}>
                    {SORTS[k]}
                    {sort === k && <span className="ss-sort-mark"> ▼</span>}
                  </th>
                ))}
                <th>出错</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => {
                const st = displayState(toSessionState(r.state), r.last_ts);
                const jumpable = Date.now() - r.last_ts <= JUMPABLE_MS;
                const agentName = AGENT_DEFS.find((a) => a.id === r.agent)?.label ?? r.agent;
                return (
                  <tr
                    key={r.session_id}
                    className={jumpable ? "clickable" : ""}
                    title={jumpable ? "点击跳转对应窗口" : undefined}
                    onClick={() => jump(r)}
                  >
                    <td>
                      <Tip
                        content={
                          st.ended
                            ? "已结束＝进程退出，或空闲超过 2 小时（与岛面板同口径）"
                            : "状态为聚合器最后已知值，每 10 秒更新"
                        }
                      >
                        <span className="ss-state">
                          <span className={`dot ${st.dot}`} />
                          {r.state === "error" ? errorReason(r.error_types) : st.label}
                        </span>
                      </Tip>
                    </td>
                    <td title={cardTitle(r.title, r.project_dir, r.session_id)}>
                      {cardTitle(r.title, r.project_dir, r.session_id)}
                    </td>
                    <td>
                      <Tip content={agentName}>
                        <span className="ss-badge" style={{ background: agentColor(r.agent) }}>
                          {AGENT_BADGE[r.agent] ?? "??"}
                        </span>
                      </Tip>
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
                    <td className="num">{r.calls}</td>
                    <td className="num">
                      <Tip
                        content={
                          r.errors > 0
                            ? `错误类型：${r.error_types ?? "未知"}`
                            : "从未出错"
                        }
                      >
                        <span className={r.errors > 0 ? "ss-err" : "ss-err0"}>{r.errors}</span>
                      </Tip>
                    </td>
                    <td
                      className="num"
                      title={
                        r.duration_ms == null
                          ? "该 Agent 的转录文件无时长字段"
                          : `模型生成时长合计 ${fmtDuration(r.duration_ms)}`
                      }
                    >
                      {fmtDuration(r.duration_ms)}
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
                            <div className="tip-dim">会话累计 · 含缓存，与账单同口径</div>
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

          {/* 分页器 */}
          <div className="ss-pager">
            <span className="ss-page-no">
              共 {page?.total ?? 0} 个会话 · 第 {pageNo} / {pageCount} 页
            </span>
            <button className="ss-btn" disabled={pageNo <= 1} onClick={() => turnPage(-1)}>
              上一页
            </button>
            <button className="ss-btn" disabled={pageNo >= pageCount} onClick={() => turnPage(1)}>
              下一页
            </button>
          </div>
        </>
      )}

      <div className="ss-foot">
        状态口径与岛面板一致（空闲超 2 小时按已结束展示）· 生成时长仅 ZCode 等转录含时长字段的
        Agent · 数据源为本地 SQLite，每次打开/刷新时查询
      </div>
    </div>
  );
}
