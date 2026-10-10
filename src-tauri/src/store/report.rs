// 报表聚合域：范围过滤/汇总/趋势/维度/热力图/预算/会话分页与明细/CSV 导出
// （2026-10-03 审查拆分：自 mod.rs 纯移动，方法体逐字节不变；领域内聚，
// Store 方法经 inherent impl 就地扩展，外部调用路径零变化）

use super::*;

// ===== 报表聚合查询（M1-R1 重构：范围档＋维度筛选＋整页快照） =====
// 设计：report_snapshot 一次组装整页数据（汇总卡/六维度条/趋势明细等）。
// 口径边界（2026-10-03 注释修正）：内部各子查询各自独立拿锁，采集写入完全
// 可以在两图之间插入——单 Mutex 保证的是互斥不是整页原子快照，极端时序下
// 页内各图数字可能相差一轮 tick（可自愈，量级＝采集周期）。刻意不改成整页
// 单锁：那会让采集 tick 被整页查询时长阻塞，得不偿失。
// 会话明细独立分页命令翻页。所有过滤值参数化绑定，维度/粒度表达式只出自
// 内部白名单 match（沿用审查 3.3"结构上不可能注入"的原则）。
// 查询统一 LEFT JOIN sessions（取项目维度，idx_usage_session 索引覆盖）
/// 四项用量合计表达式（账单口径，与岛面板/官方账单同口径）
const TOTAL_EXPR: &str = "COALESCE(u.input_tokens,0)+COALESCE(u.output_tokens,0)+COALESCE(u.cache_read_tokens,0)+COALESCE(u.cache_creation_tokens,0)";

/// 会话中心每页行数（Rust 侧权威值，随 SessionPage 下发给前端）
pub(crate) const SESSION_PAGE_SIZE: i64 = 20; // pub(crate)：mod.rs 的分页测试断言引用（拆分后跨模块可见）

/// Agent 展示默认序（2026-10-03 拖拽排序配套）：与前端 shared/types.ts 的
/// AGENT_DEFS 声明序同源同步（双端手工维护，仿 HOOKS_AGENTS 先例）。用户在
/// 设置页拖拽出的 agents_order 优先；此处仅兜底「键缺失/未覆盖的 id」
const AGENT_DISPLAY_ORDER: &[&str] = &[
    // 默认序（2026-10-09 所有者拍板）：国产在前、国际在后，各自按 label 首字母
    // 升序——与前端 shared/types.ts 的 AGENT_DEFS 声明序同源同步（双端手工维护，
    // 仿 HOOKS_AGENTS 先例）。用户拖拽出的 agents_order 优先；此处仅兜底
    // 「键缺失/未覆盖的 id」
    // —— 国产 ——
    "codebuddy",
    "kimi-code",
    "mimo-code",
    "qoder",
    "qwen-code",
    "workbuddy",
    "zcode",
    // —— 国际 ——
    "aider",
    "claude-code",
    "codex",
    "gemini",
    "copilot",
    "goose",
    "hermes",
    "openclaw",
    "opencode",
];

/// 报表筛选上下文：范围起点 + 可选时间上界 + 四个维度过滤（None=不过滤；
/// 2026-09-29 审查修复 #9b：四维度多选对比——单值改集合，IN 条件承接；
/// 空 vec＝选中了值但范围内无匹配（恒假）；项目空串成员=未知项目；
/// 供应商筛选解析为模型集合——usage_records.provider 列历史口径混杂
/// （旧前缀启发式残留），统一走模型名归因，见 provider_models_of）
struct ReportFilter {
    cutoff_ms: i64,
    /// 时间上界（不含）；None=无上界。环比查询上一周期时用 [cutoff, end) 区间
    end_ms: Option<i64>,
    agent: Option<Vec<String>>,
    project: Option<Vec<String>>,
    model: Option<Vec<String>>,
    /// 用户输入的供应商筛选值集合（仅存原始值供解析；SQL 条件由 provider_models 承接）
    provider: Option<Vec<String>>,
    /// 供应商筛选的解析产物：所选各供应商（"unknown"=归因失败组）对应的
    /// LOWER(model) 集合并集，转模型 IN 条件注入全部查询；None=未按供应商筛选
    provider_models: Option<Vec<String>>,
}

/// 范围档白名单解析 + cutoff 计算。
/// 档位固定五种：today（今日零点）｜7d/30d/90d（滚动窗口）｜all（全部历史）；
/// 白名单外的值返回 None（命令层向前端报错，防任意参数透传）
fn build_filter(
    conn: &Connection,
    range: &str,
    agent: Option<&[String]>,
    project: Option<&[String]>,
    model: Option<&[String]>,
    provider: Option<&[String]>,
) -> Option<ReportFilter> {
    let cutoff_ms = match range {
        "today" => today_start_ms(conn)?,
        "all" => 0,
        "7d" | "30d" | "90d" => {
            let d: i64 = range.trim_end_matches('d').parse().ok()?;
            now_ms() - d * 86_400_000
        }
        _ => return None,
    };
    Some(ReportFilter {
        cutoff_ms,
        end_ms: None,
        agent: agent.map(|v| v.to_vec()),
        project: project.map(|v| v.to_vec()),
        model: model.map(|v| v.to_vec()),
        provider: provider.map(|v| v.to_vec()),
        provider_models: None, // 由 report_snapshot 统一解析后注入
    })
}

/// 趋势时间桶表达式（粒度随范围自动派生，业界分析工具标准做法）：
/// today→小时（看当天节奏）｜7d/30d→日｜90d→周（30 根日柱太密）｜all→月
fn bucket_expr(range: &str) -> &'static str {
    match range {
        "today" => "strftime('%Y-%m-%d %H:00', u.ts/1000,'unixepoch','localtime')",
        "7d" | "30d" => "date(u.ts/1000,'unixepoch','localtime')",
        "90d" => "date(u.ts/1000,'unixepoch','localtime','-6 days','weekday 1')",
        _ => "strftime('%Y-%m', u.ts/1000,'unixepoch','localtime')",
    }
}

/// 维度分组表达式（白名单 match，空串=未知项目与下拉"全部"（null）区分）。
/// model 与 provider 维度统一按模型名分组——provider 的归因归并在 Rust 层
/// 完成（report_by_dim），不再读 usage_records.provider 列
fn dim_expr(dim: &str) -> &'static str {
    match dim {
        "agent" => "u.agent",
        "project" => "COALESCE(s.project_dir,'')",
        _ => "LOWER(u.model)",
    }
}

/// 公共 WHERE 片段与参数（调用方 FROM 统一为
/// usage_records u LEFT JOIN sessions s ON s.id=u.session_id）
fn filter_where(f: &ReportFilter) -> (String, Vec<rusqlite::types::Value>) {
    let mut sql = String::new();
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    sql.push_str(" WHERE u.ts >= ?");
    params.push(f.cutoff_ms.into());
    if let Some(end) = f.end_ms {
        sql.push_str(" AND u.ts < ?");
        params.push(end.into());
    }
    if let Some(agents) = &f.agent {
        // 多选（#9b）：IN 条件；空集=范围内无匹配（恒假）
        if agents.is_empty() {
            sql.push_str(" AND 1=0");
        } else {
            sql.push_str(&format!(" AND u.agent IN ({})", vec!["?"; agents.len()].join(",")));
            params.extend(agents.iter().map(|a| a.clone().into()));
        }
    }
    if let Some(ps) = &f.project {
        // 多选下已知项目与"未知项目"（空串成员）混选：IN 与 COALESCE 条件 OR 连接
        let known: Vec<&String> = ps.iter().filter(|p| !p.is_empty()).collect();
        let has_unknown = ps.iter().any(|p| p.is_empty());
        let mut parts: Vec<String> = Vec::new();
        if !known.is_empty() {
            parts.push(format!(
                "s.project_dir IN ({})",
                vec!["?"; known.len()].join(",")
            ));
            params.extend(known.into_iter().map(|p| p.clone().into()));
        }
        if has_unknown {
            // 空串=未知项目：匹配 project_dir 缺失（NULL 或空）的会话
            parts.push("COALESCE(s.project_dir,'') = ''".into());
        }
        if parts.is_empty() {
            // 选中集合为空 vec＝恒假（与单维度语义一致）
            sql.push_str(" AND 1=0");
        } else {
            sql.push_str(&format!(" AND ({})", parts.join(" OR ")));
        }
    }
    if let Some(models) = &f.model {
        if models.is_empty() {
            sql.push_str(" AND 1=0");
        } else {
            sql.push_str(&format!(
                " AND LOWER(u.model) IN ({})",
                vec!["?"; models.len()].join(",")
            ));
            // 参数侧同步小写化（原单选口径 LOWER(col)=LOWER(?) 的双侧语义）
            params.extend(models.iter().map(|m| m.to_lowercase().into()));
        }
    }
    if let Some(models) = &f.provider_models {
        // 供应商筛选 = 归因后的模型集合 IN 条件；空集 = 范围内该供应商无模型（恒假）
        if models.is_empty() {
            sql.push_str(" AND 1=0");
        } else {
            sql.push_str(&format!(
                " AND LOWER(u.model) IN ({})",
                vec!["?"; models.len()].join(",")
            ));
            params.extend(models.iter().map(|m| m.clone().into()));
        }
    }
    (sql, params)
}


impl Store {
    /// 构建报表筛选上下文（范围白名单外的档位返回 None）。
    /// 2026-09-29 起四维度多选（#9b）；会话窗口仍单选，调用点将单值包成
    /// 单元素集合传入（session_page/session_options），语义不变
    fn report_filter(
        &self,
        range: &str,
        agent: Option<&[String]>,
        project: Option<&[String]>,
        model: Option<&[String]>,
        provider: Option<&[String]>,
    ) -> Option<ReportFilter> {
        let conn = self.lock_conn();
        build_filter(&conn, range, agent, project, model, provider)
    }

    /// 汇总卡：总量/次数/会话数/活跃项目数/错误次数 + 时长/TTFT/思考占比原料 + 估算成本
    fn report_summary(
        &self,
        f: &ReportFilter,
        ov: &std::collections::HashMap<String, crate::modelcat::PriceOverride>,
    ) -> SummaryStats {
        let conn = self.lock_conn();
        let (where_sql, params) = filter_where(f);
        let sql = format!(
            "SELECT COALESCE(SUM({TOTAL_EXPR}),0), COALESCE(SUM(NOT u.is_background),0),
                    COUNT(DISTINCT u.session_id),
                    COALESCE(SUM(u.error_type IS NOT NULL),0),
                    SUM(u.duration_ms),
                    CAST(AVG(u.ttft_ms) AS INTEGER),
                    COALESCE(SUM(COALESCE(u.reasoning_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.input_tokens,0)+COALESCE(u.output_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.input_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.cache_read_tokens,0)),0)
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return SummaryStats::default();
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(SummaryStats {
                total_tokens: r.get(0)?,
                calls: r.get(1)?,
                sessions: r.get(2)?,
                errors: r.get(3)?,
                duration_ms: r.get(4)?,
                ttft_avg_ms: r.get(5)?,
                reasoning_tokens: r.get(6)?,
                billable_tokens: r.get(7)?,
                input_total: r.get(8)?,
                cache_read_total: r.get(9)?,
                est_cost: None,
                missing_price_count: 0,
            })
        });
        let mut s: SummaryStats = match rows {
            Ok(mut it) => it.next().unwrap_or(Ok(SummaryStats::default())).unwrap_or_default(),
            Err(e) => {
                log::warn!("[存储] report_summary 查询失败（本轮返回空降级）：{e}");
                SummaryStats::default()
            },
        };
        // 成本另算：单价随模型不同，须按模型分组四项乘价（不能拿总量×单一单价）。
        // 复用上方同锁连接（conn 守卫仍存活，cost_by_model 不再自行 lock_conn——
        // 非重入互斥量同线程二次加锁即自锁，M1-8 motion 锁同款教训）
        let (cost, priced, missing) = Self::cost_by_model(&conn, &where_sql, &params, ov);
        s.est_cost = if priced > 0 { Some(cost) } else { None };
        s.missing_price_count = missing;
        s
    }

    /// 按模型分组的成本估算：返回（成本合计，可计价模型数，缺价模型数）。
    /// 空范围/全缺价时 priced=0，汇总卡 est_cost 显示「--」。
    /// 连接由调用方传入（关联函数）：调用点都在已持锁的上下文内
    fn cost_by_model(
        conn: &Connection,
        where_sql: &str,
        params: &[rusqlite::types::Value],
        ov: &std::collections::HashMap<String, crate::modelcat::PriceOverride>,
    ) -> (f64, usize, usize) {
        let sql = format!(
            "SELECT LOWER(u.model) AS m,
                    COALESCE(SUM(COALESCE(u.input_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.output_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.cache_read_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.cache_creation_tokens,0)),0)
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}
             GROUP BY m"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return (0.0, 0, 0);
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        });
        let rows: Vec<_> = match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(e) => {
                log::warn!("[存储] cost_by_model 查询失败（本轮返回空降级）：{e}");
                vec![]
            },
        };
        let mut cost = 0.0;
        let mut priced = 0usize;
        let mut missing = 0usize;
        for (m, input, output, cr, cc) in rows {
            match crate::modelcat::resolve_price(&m, ov) {
                Some(p) => {
                    priced += 1;
                    cost += crate::modelcat::cost_of(&p, &TokenBreakdown { input, output, cache_read: cr, cache_creation: cc });
                }
                None => missing += 1,
            }
        }
        (cost, priced, missing)
    }

    /// 趋势：按范围派生粒度分桶，四项用量 + 次数 + 生成时长 + 估算成本。
    /// SQL 按「桶 × 模型」双分组，成本在 Rust 层逐模型乘价后归并回桶
    /// （BTreeMap 键序 = 桶字典序 = 时间序，与原 ORDER BY 等价）
    fn report_trend(
        &self,
        f: &ReportFilter,
        range: &str,
        ov: &std::collections::HashMap<String, crate::modelcat::PriceOverride>,
    ) -> Vec<TrendRow> {
        let conn = self.lock_conn();
        let bucket = bucket_expr(range);
        let (where_sql, params) = filter_where(f);
        let sql = format!(
            "SELECT {bucket} AS b, LOWER(u.model) AS m,
                    COALESCE(SUM(COALESCE(u.input_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.output_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.cache_read_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.cache_creation_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.reasoning_tokens,0)),0),
                    COALESCE(SUM(NOT u.is_background),0),
                    SUM(u.duration_ms)
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}
             GROUP BY b, m"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return vec![];
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<i64>>(8)?,
            ))
        });
        let rows: Vec<_> = match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(e) => {
                log::warn!("[存储] report_trend 查询失败（本轮返回空降级）：{e}");
                vec![]
            },
        };
        // 逐行乘价并归并回桶：Option 的时长按 Some 累加（任一桶×模型有时长即计入）
        let mut acc: std::collections::BTreeMap<String, (i64, i64, i64, i64, i64, i64, Option<i64>, f64)> =
            std::collections::BTreeMap::new();
        for (b, m, input, output, cr, cc, reasoning, calls, duration) in rows {
            let cost = crate::modelcat::resolve_price(&m, ov)
                .map(|p| crate::modelcat::cost_of(&p, &TokenBreakdown { input, output, cache_read: cr, cache_creation: cc }))
                .unwrap_or(0.0);
            let e = acc.entry(b).or_insert((0, 0, 0, 0, 0, 0, None, 0.0));
            e.0 += input;
            e.1 += output;
            e.2 += cr;
            e.3 += cc;
            e.4 += reasoning;
            e.5 += calls;
            e.6 = match (e.6, duration) {
                (None, d) => d,
                (Some(a), Some(b)) => Some(a + b),
                (Some(a), None) => Some(a),
            };
            e.7 += cost;
        }
        acc.into_iter()
            .map(|(bucket, (input, output, cr, cc, reasoning, calls, duration, cost))| TrendRow {
                bucket,
                input,
                output,
                cache_read: cr,
                cache_creation: cc,
                reasoning,
                calls,
                duration_ms: duration,
                cost,
            })
            .collect()
    }

    /// 趋势维度切片（#9a）：时间桶×维度值分组的 token 合计（Agent/模型两种），
    /// 供趋势图切"按 Agent／按模型"堆叠；与 report_trend 同一套 WHERE 与分桶
    fn report_trend_slices(&self, f: &ReportFilter, range: &str, dim: &str) -> Vec<TrendSlice> {
        let conn = self.lock_conn();
        let bucket = bucket_expr(range);
        let (where_sql, params) = filter_where(f);
        let sql = format!(
            "SELECT {bucket} AS b, {dim_expr} AS label,
                    COALESCE(SUM({TOTAL_EXPR}),0)
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}
             GROUP BY b, label",
            dim_expr = dim_expr(dim)
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return vec![];
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(TrendSlice {
                bucket: r.get(0)?,
                label: r.get(1)?,
                total: r.get(2)?,
            })
        });
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(e) => {
                log::warn!("[存储] report_trend_slices 查询失败（本轮返回空降级）：{e}");
                vec![]
            },
        }
    }

    /// 维度分组（Agent/项目/模型/供应商，dim 白名单见 dim_expr）。
    /// SQL 按「维度 × 模型」双分组取四项 token，成本在 Rust 层按模型单价
    /// 逐行乘价后归并到维度（单价随模型不同，四项总量×单一单价不成立）；
    /// provider 维度在同一查询上以归因函数作归并键（存量 provider 列不再参与）
    fn report_by_dim(
        &self,
        f: &ReportFilter,
        dim: &str,
        ov: &std::collections::HashMap<String, crate::modelcat::PriceOverride>,
    ) -> Vec<SliceUsage> {
        let conn = self.lock_conn();
        let group = dim_expr(dim);
        let (where_sql, params) = filter_where(f);
        let sql = format!(
            "SELECT {group} AS label, LOWER(u.model) AS m,
                    COALESCE(SUM({TOTAL_EXPR}),0), COALESCE(SUM(NOT u.is_background),0),
                    COALESCE(SUM(COALESCE(u.input_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.output_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.cache_read_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.cache_creation_tokens,0)),0)
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}
             GROUP BY label, m"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return vec![];
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
            ))
        });
        let rows: Vec<_> = match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(e) => {
                log::warn!("[存储] report_by_dim 查询失败（本轮返回空降级）：{e}");
                vec![]
            },
        };
        // 归并键：provider 维度 = 模型名归因（失败归 "unknown"，与筛选下拉同口径）；
        // 其余维度 = 分组标签本身
        let key_of = |label: &str, m: &str| -> String {
            if dim == "provider" {
                crate::modelcat::resolve_provider(m).unwrap_or_else(|| "unknown".into())
            } else {
                label.to_string()
            }
        };
        let mut acc: std::collections::HashMap<String, (i64, i64, f64)> = std::collections::HashMap::new();
        for (label, m, total, calls, input, output, cr, cc) in rows {
            let cost = crate::modelcat::resolve_price(&m, ov)
                .map(|p| crate::modelcat::cost_of(&p, &TokenBreakdown { input, output, cache_read: cr, cache_creation: cc }))
                .unwrap_or(0.0);
            let e = acc.entry(key_of(&label, &m)).or_insert((0, 0, 0.0));
            e.0 += total;
            e.1 += calls;
            e.2 += cost;
        }
        let mut out: Vec<SliceUsage> = acc
            .into_iter()
            .map(|(label, (total, calls, cost))| SliceUsage { label, total, calls, cost })
            .collect();
        // 归并后排序（原 SQL 的 ORDER BY 随双分组失效）：总量降序、标签升序稳定
        out.sort_by(|a, b| b.total.cmp(&a.total).then(a.label.cmp(&b.label)));
        out
    }

    /// 周×小时热力图（本机时区，token 与次数双指标）
    fn report_heatmap(&self, f: &ReportFilter) -> Vec<HeatCell> {
        let conn = self.lock_conn();
        let (where_sql, params) = filter_where(f);
        let sql = format!(
            "SELECT CAST(strftime('%w', u.ts/1000,'unixepoch','localtime') AS INTEGER),
                    CAST(strftime('%H', u.ts/1000,'unixepoch','localtime') AS INTEGER),
                    COALESCE(SUM({TOTAL_EXPR}),0), COALESCE(SUM(NOT u.is_background),0)
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}
             GROUP BY 1,2"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return vec![];
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(HeatCell {
                weekday: r.get(0)?,
                hour: r.get(1)?,
                total: r.get(2)?,
                calls: r.get(3)?,
            })
        });
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(e) => {
                log::warn!("[存储] report_heatmap 查询失败（本轮返回空降级）：{e}");
                vec![]
            },
        }
    }

    /// 筛选下拉选项（仅按范围过滤，维度互不遮蔽）
    fn report_options(&self, f: &ReportFilter) -> FilterOptions {
        let conn = self.lock_conn();
        let mut out = FilterOptions::default();
        let lists: [(&str, &str); 3] = [
            ("agent", "SELECT DISTINCT u.agent FROM usage_records u WHERE u.ts >= ? ORDER BY 1"),
            ("project", "SELECT DISTINCT COALESCE(s.project_dir,'') FROM usage_records u
                         LEFT JOIN sessions s ON s.id = u.session_id WHERE u.ts >= ? ORDER BY 1"),
            ("model", "SELECT DISTINCT LOWER(u.model) FROM usage_records u WHERE u.ts >= ? ORDER BY 1"),
        ];
        for (key, sql) in lists {
            let Ok(mut stmt) = conn.prepare(sql) else {
                continue;
            };
            let rows = stmt.query_map([f.cutoff_ms], |r| r.get::<_, String>(0));
            if let Ok(it) = rows {
                let vals: Vec<String> = it.filter_map(|x| x.ok()).collect();
                match key {
                    "agent" => out.agents = vals,
                    "project" => out.projects = vals,
                    // 供应商选项由模型名归因生成（M3-1，与维度条/筛选同口径不读
                    // provider 列）——四轮审查：原先 provider 是逐字节相同的第二条
                    // SQL 再扫一遍全表，现直接复用 model 结果集在 Rust 侧归因
                    _ => {
                        let mut set: std::collections::BTreeSet<String> = vals
                            .iter()
                            .map(|m| {
                                crate::modelcat::resolve_provider(m)
                                    .unwrap_or_else(|| "unknown".into())
                            })
                            .collect();
                        // BTreeSet 天然去重排序；"unknown" 组排尾部更贴近语义
                        let unknown = set.take("unknown");
                        out.providers = set.into_iter().collect();
                        if unknown.is_some() {
                            out.providers.push("unknown".to_string());
                        }
                        out.models = vals;
                    }
                }
            }
        }
        // 先释放本函数持有的连接锁再调 list_provider_accounts（其内部再次
        // lock 同一 Mutex）——MutexGuard 活到函数末尾的持锁自锁坑，HANDOFF
        // 踩坑记录同款（2026-09-29 本轮修复：曾致三个报表测试 60s 挂起）
        drop(conn);
        // Agent 下拉按用户自定义序展示（agents_order，设置页拖拽排序）：
        // 读取同样要走 get_setting（内部再拿锁），必须在 drop(conn) 之后。
        // 键缺失/解析失败→空表，由默认序补齐（前端 parseAgentOrder 同款语义：
        // 已存序优先、未覆盖的新 Agent 按默认序补尾）；两者都未登记的 id
        // （历史旧家数据）不参与 position 匹配，稳定排序保持 SQL 字母序沉底
        let mut order: Vec<String> = self
            .get_setting("agents_order")
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default();
        for id in AGENT_DISPLAY_ORDER {
            if !order.iter().any(|o| o == id) {
                order.push(id.to_string());
            }
        }
        out.agents
            .sort_by_key(|a| order.iter().position(|o| o == a).unwrap_or(usize::MAX));
        // 额度曲线实例标签（#8）：实例表全量投影（含停用），厂商名走
        // runtime_kind_name 双站变体投影（2026-10-10）——缺厂商兜底 kind_id，
        // 与岛/托盘实例视图同口径
        out.quota_accounts = self
            .list_provider_accounts()
            .into_iter()
            .map(|a| QuotaAccountLabel {
                kind_name: crate::provider::runtime_kind_name(
                    &a.kind_id,
                    a.base_override.as_deref(),
                ),
                provider: a.kind_id,
                alias: if a.alias.is_empty() { None } else { Some(a.alias) },
                account_id: a.id,
            })
            .collect();
        out
    }

    /// 错误类型分布（限流/取消/网络等；仅统计 error_type 非空的记录）
    fn report_by_error(&self, f: &ReportFilter) -> Vec<SliceUsage> {
        let conn = self.lock_conn();
        let (where_sql, params) = filter_where(f);
        let sql = format!(
            "SELECT u.error_type AS label, COUNT(*)
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}
               AND u.error_type IS NOT NULL
             GROUP BY label ORDER BY 2 DESC"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return vec![];
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(SliceUsage {
                label: r.get(0)?,
                total: r.get(1)?,
                calls: r.get(1)?,
                cost: 0.0, // 错误分布无成本语义
            })
        });
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(e) => {
                log::warn!("[存储] report_by_error 查询失败（本轮返回空降级）：{e}");
                vec![]
            },
        }
    }

    /// 按模型平均首字延迟（仅统计上报了 ttft 的前台调用；
    /// total=平均毫秒、calls=有首字记录的调用条数，前端据此展示置信度）
    fn report_ttft_by_model(&self, f: &ReportFilter) -> Vec<SliceUsage> {
        let conn = self.lock_conn();
        let (where_sql, params) = filter_where(f);
        let sql = format!(
            "SELECT LOWER(u.model) AS label, CAST(AVG(u.ttft_ms) AS INTEGER), COUNT(u.ttft_ms)
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}
               AND u.ttft_ms IS NOT NULL AND u.is_background = 0
             GROUP BY label ORDER BY 2 DESC"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return vec![];
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(SliceUsage {
                label: r.get(0)?,
                total: r.get(1)?,
                calls: r.get(2)?,
                cost: 0.0, // 首字延迟口径无成本语义
            })
        });
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(e) => {
                log::warn!("[存储] report_ttft_by_model 查询失败（本轮返回空降级）：{e}");
                vec![]
            },
        }
    }

    /// 消费预算用量（#23）：今日与本月（自然日/自然月，本机时区）的估算成本
    /// （USD，与报表汇总卡同源算法——逐模型乘当前单价；无可计价模型为 None）。
    /// today 走公共 report_snapshot 的 today 档；本月自建月起点 filter
    pub fn budget_usage(&self) -> Option<(Option<f64>, Option<f64>)> {
        let conn = self.lock_conn();
        let today_cutoff = today_start_ms(&conn)?;
        let month_cutoff = month_start_ms(&conn)?;
        drop(conn);
        let ov = self.list_price_overrides();
        // 轻量直查（2026-10-03 审查修复）：today 档原走 report_snapshot 整页快照
        // （六维度条＋趋势明细全算）只为取一个 est_cost，报表页挂载即双重整页
        // 查询；改镜像月度分支的 report_summary 直查，两档同款
        let cost = |cutoff_ms: i64| {
            let f = ReportFilter {
                cutoff_ms,
                end_ms: None,
                agent: None,
                project: None,
                model: None,
                provider: None,
                provider_models: None,
            };
            self.report_summary(&f, &ov).est_cost
        };
        Some((cost(today_cutoff), cost(month_cutoff)))
    }

    /// 年度热力图（2026-09-29 审查新增 #24）：近 365 天逐日 token/次数聚合
    /// （GitHub 风格贡献图的用量版）；只回有数据的日子，空日由前端补零。
    /// 与周×小时热力图互补：那个看"一天内节奏"，这个看"全年坚持度/爆发日"。
    /// 恒近 365 天全量，不随报表页范围/维度筛选联动（2026-09-29 评审后暂缓：
    /// 范围联动下短档只有一两列格子、all 档轴起点口径含糊，等口径想清楚再做）
    pub fn year_heatmap(&self) -> Vec<(String, i64, i64)> {
        let conn = self.lock_conn();
        let sql = format!(
            "SELECT date(u.ts/1000,'unixepoch','localtime'),
                    COALESCE(SUM({TOTAL_EXPR}),0),
                    COALESCE(SUM(NOT u.is_background),0)
             FROM usage_records u
             WHERE u.ts >= (CAST(strftime('%s','now','localtime','-364 days','start of day','utc') AS INTEGER)*1000)
             GROUP BY 1 ORDER BY 1"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return vec![];
        };
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        });
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(e) => {
                log::warn!("[存储] year_heatmap 查询失败（本轮返回空降级）：{e}");
                vec![]
            },
        }
    }

    /// 整页报表快照（一次调用返回全部图数据）。
    /// 2026-09-29 起：四维度多选（#9b，IN 条件）＋趋势维度切片（#9a，
    /// 趋势图可切"按 Agent／按模型"堆叠）
    pub fn report_snapshot(
        &self,
        range: &str,
        agent: Option<&[String]>,
        project: Option<&[String]>,
        model: Option<&[String]>,
        provider: Option<&[String]>,
    ) -> Option<ReportSnapshot> {
        let mut f = self.report_filter(range, agent, project, model, provider)?;
        // 供应商筛选解析（M3-1）：归因产物（模型集合并集）注入 filter，替代旧
        // provider 列条件——单次分桶后多选取并集（#9b）
        if let Some(ps) = f.provider.take() {
            let buckets = self.provider_models_map(&f);
            let mut union: Vec<String> = Vec::new();
            for p in &ps {
                if let Some(ms) = buckets.get(p) {
                    for m in ms {
                        if !union.contains(m) {
                            union.push(m.clone());
                        }
                    }
                }
            }
            f.provider_models = Some(union);
        }
        let overrides = self.list_price_overrides();
        // 环比基准：上一等长周期 [start, end)。today→昨天零点起；
        // 7d/30d/90d→紧邻上一段；all→全部历史无上期，不比
        let prev_window: Option<(i64, i64)> = match range {
            "today" => {
                let t = today_start_ms(&self.lock_conn())?;
                Some((t - 86_400_000, t))
            }
            "7d" | "30d" | "90d" => {
                let span = range.trim_end_matches('d').parse::<i64>().ok()? * 86_400_000;
                Some((now_ms() - 2 * span, now_ms() - span))
            }
            _ => None,
        };
        let prev_summary = prev_window.map(|(start, end)| {
            self.report_summary(
                &ReportFilter {
                    cutoff_ms: start,
                    end_ms: Some(end),
                    agent: f.agent.clone(),
                    project: f.project.clone(),
                    model: f.model.clone(),
                    provider: None,
                    provider_models: f.provider_models.clone(),
                },
                &overrides,
            )
        });
        // 额度历史与用量筛选无关（额度按供应商计），仅与范围档同窗口
        let quota_curve = self.quota_history(f.cutoff_ms);
        Some(ReportSnapshot {
            summary: self.report_summary(&f, &overrides),
            prev_summary,
            trend: self.report_trend(&f, range, &overrides),
            trend_agent_slices: self.report_trend_slices(&f, range, "agent"),
            trend_model_slices: self.report_trend_slices(&f, range, "model"),
            by_agent: self.report_by_dim(&f, "agent", &overrides),
            by_project: self.report_by_dim(&f, "project", &overrides),
            by_model: self.report_by_dim(&f, "model", &overrides),
            by_provider: self.report_by_dim(&f, "provider", &overrides),
            by_error: self.report_by_error(&f),
            ttft_by_model: self.report_ttft_by_model(&f),
            quota_curve,
            heatmap: self.report_heatmap(&f),
            options: self.report_options(&f),
        })
    }

    /// 供应商筛选解析（06 §6.2）：usage_records.provider 列历史口径混杂
    /// （旧前缀启发式写入，且 qwen 曾归 "alibaba"），统一改走模型名归因——
    /// 在「范围＋其余三维筛选」下取一次 DISTINCT model 并按归因结果分桶
    /// （归因失败入 "unknown" 桶），供 filter_where 生成 IN 条件。
    /// 四轮审查：原先按目标供应商逐家扫描（多选 N 家＝同结果集全范围扫 N 遍），
    /// 现单次扫描分桶，多选只查表一次
    fn provider_models_map(&self, f: &ReportFilter) -> std::collections::HashMap<String, Vec<String>> {
        let conn = self.lock_conn();
        let (where_sql, params) = filter_where(f);
        let sql = format!(
            "SELECT DISTINCT LOWER(u.model) FROM usage_records u
             LEFT JOIN sessions s ON s.id = u.session_id{where_sql}"
        );
        let mut map: std::collections::HashMap<String, Vec<String>> = Default::default();
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return map;
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            r.get::<_, String>(0)
        });
        match rows {
            Ok(it) => {
                for m in it.filter_map(|x| x.ok()) {
                    let key = crate::modelcat::resolve_provider(m.as_str())
                        .unwrap_or_else(|| "unknown".into());
                    map.entry(key).or_default().push(m);
                }
            },
            Err(e) => log::warn!("[存储] provider_models_map 查询失败（本轮返回空降级）：{e}"),
        }
        map
    }

    /// 会话中心筛选下拉选项（轻查询：会话窗口不需要整页报表快照，
    /// 只要 Agent/项目/模型三张选项表；只受范围影响、不受维度筛选影响，
    /// 保证任意筛选组合下选项仍然齐全可切换）
    pub fn session_options(&self, range: &str) -> Option<FilterOptions> {
        // 会话窗口暂无供应商筛选，第 5 参固定 None
        let f = self.report_filter(range, None, None, None, None)?;
        Some(self.report_options(&f))
    }

    /// 会话中心分页（会话窗口主查询，M1-10）：范围＋三维度筛选同报表口径，
    /// 另加状态档（all｜active｜ended｜errored）/关键字（标题、项目路径模糊匹配）/
    /// 排序键（recent｜tokens｜calls｜duration，白名单映射防注入）/
    /// 每页行数（0 或负值回退默认，钳制 ≤200 限制 LIMIT 注入面）。
    /// 结构=内层按会话聚合子查询＋外层套状态/关键字过滤。
    /// 状态口径（2026-09-21 冻结修复）：active = 状态非 offline 且最近活动在
    /// ENDED_AFTER_MS 内——原来三态（working/waiting/error）无时间条件，会话离开
    /// 90 天观测列表后 sessions.state 冻结在最后值会被永久当"活跃"；统一时间窗后
    /// 与前端 shared/sessionDisplay.ts 的 ENDED_AFTER_MS 语义对齐；
    /// ended=其余（offline、空闲超时、冻结）。errored=出过错。
    /// 状态判定依赖聚合值 MAX(u.ts)，故过滤必须套在聚合之后
    pub fn session_page(
        &self,
        range: &str,
        agent: Option<&str>,
        project: Option<&str>,
        model: Option<&str>,
        status: &str,
        keyword: &str,
        sort: &str,
        page_size: i64,
        offset: i64,
    ) -> Option<SessionPage> {
        // 每页行数：非法值回退默认，上限 200
        let page_size = if page_size <= 0 { SESSION_PAGE_SIZE } else { page_size.min(200) };
        // 状态档与排序键先行白名单校验（未知值返回 None，命令层向前端报错）
        let status_sql: &str = match status {
            "all" => "",
            "active" => " AND (state != 'offline' AND last_ts >= ?)",
            "ended" => " AND NOT (state != 'offline' AND last_ts >= ?)",
            "errored" => " AND errors > 0",
            _ => return None,
        };
        let order_sql: &str = match sort {
            "recent" => "last_ts DESC",
            "tokens" => "total_tokens DESC",
            "calls" => "calls DESC",
            "duration" => "duration_ms DESC",
            _ => return None,
        };
        // 会话窗口保持单选语义：单值包成单元素集合进多选 filter（#9b 改造适配）
        let one = |v: Option<&str>| v.map(|s| vec![s.to_string()]);
        let f = self.report_filter(
            range,
            one(agent).as_deref(),
            one(project).as_deref(),
            one(model).as_deref(),
            None,
        )?;
        let conn = self.lock_conn();
        let (where_sql, mut params) = filter_where(&f);
        // 内层子查询：按会话聚合（别名供外层过滤/排序引用，避免脆弱的列号）；
        // calls 不含后台校准行（cost-state 差值非真实调用）
        let inner = format!(
            "SELECT u.session_id, COALESCE(s.agent, u.agent) AS agent,
                    COALESCE(s.state,'offline') AS state, s.model, s.project_dir, s.title,
                    MIN(u.ts) AS first_ts, MAX(u.ts) AS last_ts,
                    COALESCE(SUM(NOT u.is_background),0) AS calls,
                    COALESCE(SUM(COALESCE(u.input_tokens,0)),0) AS input_tokens,
                    COALESCE(SUM(COALESCE(u.output_tokens,0)),0) AS output_tokens,
                    COALESCE(SUM(COALESCE(u.cache_read_tokens,0)),0) AS cache_read_tokens,
                    COALESCE(SUM(COALESCE(u.cache_creation_tokens,0)),0) AS cache_creation_tokens,
                    COALESCE(SUM(COALESCE(u.reasoning_tokens,0)),0) AS reasoning_tokens,
                    COALESCE(SUM({TOTAL_EXPR}),0) AS total_tokens,
                    SUM(u.duration_ms) AS duration_ms,
                    CAST(AVG(u.ttft_ms) AS INTEGER) AS ttft_avg_ms,
                    COALESCE(SUM(u.error_type IS NOT NULL),0) AS errors,
                    GROUP_CONCAT(DISTINCT u.error_type) AS error_types
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}
             GROUP BY u.session_id"
        );
        // 关键字：转义 LIKE 通配符后模糊匹配标题与项目全路径（参数化，空/纯空白=不搜）
        let kw = keyword.trim();
        if !kw.is_empty() {
            let pat = format!("%{}%", escape_like(kw));
            params.push(pat.clone().into());
            params.push(pat.into());
        }
        // active/ended 的时间阈值参数（NOW − 2h），排在关键字之后
        if status == "active" || status == "ended" {
            params.push((now_ms() - ENDED_AFTER_MS).into());
        }
        // 占位顺序＝关键字两枚 → 状态一枚，与下方 SQL 文本顺序一致
        let kw_sql = if kw.is_empty() {
            ""
        } else {
            " AND (COALESCE(title,'') LIKE ? ESCAPE '\\' OR COALESCE(project_dir,'') LIKE ? ESCAPE '\\')"
        };
        // 总数：同一聚合子查询＋同一套过滤，口径与行查询完全一致
        let count_sql =
            format!("SELECT COUNT(*) FROM ({inner}) WHERE 1=1{kw_sql}{status_sql}");
        let total: i64 = conn
            .prepare(&count_sql)
            .and_then(|mut s| s.query_row(rusqlite::params_from_iter(params.iter()), |r| r.get(0)))
            .unwrap_or(0);
        // 行查询：LIMIT/OFFSET 参数追加在过滤参数之后
        let mut all_params = params;
        all_params.push(page_size.into());
        all_params.push(offset.into());
        let rows_sql =
            format!("SELECT * FROM ({inner}) WHERE 1=1{kw_sql}{status_sql} ORDER BY {order_sql} LIMIT ? OFFSET ?");
        let mut rows = Vec::new();
        if let Ok(mut stmt) = conn.prepare(&rows_sql) {
            let mapped = stmt.query_map(rusqlite::params_from_iter(all_params.iter()), |r| {
                Ok(SessionRow {
                    session_id: r.get("session_id")?,
                    agent: r.get("agent")?,
                    state: r.get("state")?,
                    model: r.get("model")?,
                    project_dir: r.get("project_dir")?,
                    title: r.get("title")?,
                    first_ts: r.get("first_ts")?,
                    last_ts: r.get("last_ts")?,
                    calls: r.get("calls")?,
                    input_tokens: r.get("input_tokens")?,
                    output_tokens: r.get("output_tokens")?,
                    cache_read_tokens: r.get("cache_read_tokens")?,
                    cache_creation_tokens: r.get("cache_creation_tokens")?,
                    reasoning_tokens: r.get("reasoning_tokens")?,
                    total_tokens: r.get("total_tokens")?,
                    duration_ms: r.get("duration_ms")?,
                    ttft_avg_ms: r.get("ttft_avg_ms")?,
                    errors: r.get("errors")?,
                    error_types: r.get("error_types")?,
                })
            });
            if let Ok(it) = mapped {
                rows = it.filter_map(|x| x.ok()).collect();
            }
        }
        Some(SessionPage {
            total,
            page_size,
            rows,
        })
    }

    /// 会话详情（M1-11 抽屉）：单会话的调用流水 + 状态事件时间线。
    /// 调用流水倒序（最新在上），上限 500 条防极端会话拖爆前端；
    /// 状态事件的 session_id 存在两代口径——hook 事件按原始 id 记账、
    /// 快照按 "{agent}:{id}" 命名空间 id——两个都查才不漏；
    /// 另查两表真实总数（COUNT），列表被 LIMIT 截断时前端诚实展示「N / 共 M」
    pub fn session_detail(&self, session_id: &str) -> SessionDetail {
        let conn = self.lock_conn();
        let mut detail = SessionDetail::default();
        // 原始 id：剥掉 "{agent}:" 前缀（无前缀时原样，防御）
        let raw_id = session_id.split_once(':').map(|x| x.1).unwrap_or(session_id);
        // 真实总数：一条查询带两个标量子查询（状态事件同样兼容两代 session_id 口径）；
        // 调用总数排除后台校准行（cost-state 差值非真实调用，流水同样不展示）
        if let Ok(mut stmt) = conn.prepare(
            "SELECT (SELECT COUNT(*) FROM usage_records
                      WHERE session_id = ?1 AND is_background = 0),
                    (SELECT COUNT(*) FROM status_events WHERE session_id = ?1 OR session_id = ?2)",
        ) {
            if let Ok(mut it) = stmt.query(rusqlite::params![session_id, raw_id]) {
                if let Ok(Some(row)) = it.next() {
                    detail.calls_total = row.get(0).unwrap_or(0);
                    detail.events_total = row.get(1).unwrap_or(0);
                }
            }
        }
        if let Ok(mut stmt) = conn.prepare(
            "SELECT ts, model,
                    COALESCE(input_tokens,0), COALESCE(output_tokens,0),
                    COALESCE(reasoning_tokens,0),
                    COALESCE(cache_read_tokens,0), COALESCE(cache_creation_tokens,0),
                    duration_ms, ttft_ms, error_type
             FROM usage_records WHERE session_id = ?1 AND is_background = 0 ORDER BY ts DESC LIMIT 500",
        ) {
            if let Ok(it) = stmt.query_map([session_id], |r| {
                Ok(SessionCallRow {
                    ts: r.get(0)?,
                    model: r.get(1)?,
                    input_tokens: r.get(2)?,
                    output_tokens: r.get(3)?,
                    reasoning_tokens: r.get(4)?,
                    cache_read_tokens: r.get(5)?,
                    cache_creation_tokens: r.get(6)?,
                    duration_ms: r.get(7)?,
                    ttft_ms: r.get(8)?,
                    error_type: r.get(9)?,
                })
            }) {
                detail.calls = it.filter_map(|x| x.ok()).collect();
            }
        }
        // 状态事件查询：命名空间 id 与原始 id 双口径都命中才不漏
        if let Ok(mut stmt) = conn.prepare(
            "SELECT ts, hook, payload FROM status_events
             WHERE session_id = ?1 OR session_id = ?2 ORDER BY ts DESC LIMIT 200",
        ) {
            if let Ok(it) = stmt.query_map(rusqlite::params![session_id, raw_id], |r| {
                Ok(SessionEventRow {
                    ts: r.get(0)?,
                    hook: r.get(1)?,
                    payload: r.get(2)?,
                })
            }) {
                detail.events = it.filter_map(|x| x.ok()).collect();
            }
        }
        detail
    }

    /// 按当前范围＋筛选导出报表 CSV（2026-09-29 审查修复 #11：会话 CSV 迁走后
    /// 报表页无任何导出）。与 report_snapshot 同一次查询——导出与页面所见口径
    /// 100% 一致；结构＝表头（范围与筛选快照）＋汇总卡（含环比）＋六个维度条
    /// ＋趋势明细。纯函数只产字符串，写文件由命令层负责；UTF-8 BOM 同会话 CSV
    pub fn build_report_csv(
        &self,
        range: &str,
        agent: Option<&[String]>,
        project: Option<&[String]>,
        model: Option<&[String]>,
        provider: Option<&[String]>,
    ) -> Option<String> {
        let snap = self.report_snapshot(range, agent, project, model, provider)?;
        // 生成时间走 SQLite 本地时区格式化（与页面「数据截至」同源口径）
        let gen_at: String = self
            .lock_conn()
            .query_row("SELECT strftime('%Y-%m-%d %H:%M:%S','now','localtime')", [], |r| r.get(0))
            .unwrap_or_default();
        let range_label = match range {
            "today" => "今日",
            "7d" => "近 7 天",
            "30d" => "近 30 天",
            "90d" => "近 90 天",
            "all" => "全部",
            _ => range,
        };
        let mut s = String::from("\u{FEFF}");
        let fmt_filter = |v: Option<&[String]>| -> String {
            match v {
                None => "全部".into(),
                Some(list) if list.is_empty() => "（无匹配）".into(),
                Some(list) => list.join("、"),
            }
        };
        s.push_str(&format!(
            "报表导出\r\n范围,{range_label}\r\nAgent,{}\r\n项目,{}\r\n模型,{}\r\n供应商,{}\r\n生成时间,{}\r\n\r\n",
            csv_cell(&fmt_filter(agent)),
            csv_cell(&fmt_filter(project)),
            csv_cell(&fmt_filter(model)),
            csv_cell(&fmt_filter(provider)),
            csv_cell(&gen_at),
        ));
        // 汇总卡（与页面九卡同源字段；环比＝(本期−上期)/上期，上期缺失为空）
        let delta = |cur: f64, prev: Option<f64>| -> String {
            match prev {
                Some(p) if p != 0.0 => format!("{:+.1}%", (cur - p) / p * 100.0),
                _ => String::new(),
            }
        };
        let prev = snap.prev_summary.as_ref();
        s.push_str("[汇总]\r\n指标,数值,环比\r\n");
        let sum = &snap.summary;
        let rows: Vec<(String, String, String)> = vec![
            ("总 Token".into(), sum.total_tokens.to_string(),
                delta(sum.total_tokens as f64, prev.map(|p| p.total_tokens as f64))),
            ("估算成本（美元）".into(),
                sum.est_cost.map(|c| format!("{c:.4}")).unwrap_or_else(|| "--".into()),
                delta(sum.est_cost.unwrap_or(0.0), prev.and_then(|p| p.est_cost))),
            ("调用次数".into(), sum.calls.to_string(),
                delta(sum.calls as f64, prev.map(|p| p.calls as f64))),
            ("生成时长（毫秒）".into(), sum.duration_ms.map(|d| d.to_string()).unwrap_or_else(|| "--".into()),
                delta(sum.duration_ms.unwrap_or(0) as f64, prev.and_then(|p| p.duration_ms).map(|d| d as f64))),
            ("平均首字延迟（毫秒）".into(), sum.ttft_avg_ms.map(|d| d.to_string()).unwrap_or_else(|| "--".into()),
                delta(sum.ttft_avg_ms.unwrap_or(0) as f64, prev.and_then(|p| p.ttft_avg_ms).map(|d| d as f64))),
            ("思考占比".into(),
                if sum.billable_tokens > 0 { format!("{:.1}%", sum.reasoning_tokens as f64 / sum.billable_tokens as f64 * 100.0) } else { "--".into() },
                String::new()),
            ("缓存命中率".into(),
                if sum.input_total > 0 { format!("{:.1}%", sum.cache_read_total as f64 / sum.input_total as f64 * 100.0) } else { "--".into() },
                String::new()),
            ("会话数".into(), sum.sessions.to_string(),
                delta(sum.sessions as f64, prev.map(|p| p.sessions as f64))),
            ("出错次数".into(), sum.errors.to_string(),
                delta(sum.errors as f64, prev.map(|p| p.errors as f64))),
        ];
        for (k, v, d) in rows {
            s.push_str(&format!("{},{},{}\r\n", csv_cell(&k), csv_cell(&v), csv_cell(&d)));
        }
        // 六个维度条
        let dims: [(&str, &Vec<SliceUsage>); 6] = [
            ("按 Agent", &snap.by_agent),
            ("按项目", &snap.by_project),
            ("按模型", &snap.by_model),
            ("按供应商", &snap.by_provider),
            ("按错误类型", &snap.by_error),
            ("按模型首字延迟", &snap.ttft_by_model),
        ];
        for (title, list) in dims {
            s.push_str(&format!("\r\n[{title}]\r\n名称,Token/数值,次数,估算成本（美元）\r\n"));
            for r in list {
                s.push_str(&format!(
                    "{},{},{},{}\r\n",
                    csv_cell(&r.label),
                    r.total,
                    r.calls,
                    format!("{:.4}", r.cost)
                ));
            }
        }
        // 趋势明细（四项拆解＋次数/时长/成本，与堆叠图同源）
        s.push_str("\r\n[趋势明细]\r\n时间桶,输入,输出,缓存读,缓存写,思考,次数,时长（毫秒）,估算成本（美元）\r\n");
        for t in &snap.trend {
            s.push_str(&format!(
                "{},{},{},{},{},{},{},{},{}\r\n",
                csv_cell(&t.bucket),
                t.input,
                t.output,
                t.cache_read,
                t.cache_creation,
                t.reasoning,
                t.calls,
                t.duration_ms.map(|d| d.to_string()).unwrap_or_default(),
                format!("{:.4}", t.cost)
            ));
        }
        Some(s)
    }

    /// 按当前范围＋筛选＋状态档＋关键字导出会话列表 CSV（M1-10 随会话窗口迁移；
    /// 与 session_page 同一套子查询与占位顺序，所见即所得——排序也保留）。
    /// 纯函数只产字符串，写文件/落路径由命令层负责（可单测）；
    /// 带 UTF-8 BOM——Excel 直接打开中文表头不乱码；
    /// 时间列由 SQLite 按本机时区格式化（与页面口径同源）
    pub fn build_sessions_csv(
        &self,
        range: &str,
        agent: Option<&str>,
        project: Option<&str>,
        model: Option<&str>,
        status: &str,
        keyword: &str,
        sort: &str,
    ) -> Option<String> {
        // 白名单校验与 session_page 同款（状态口径同其 2026-09-21 冻结修复版）
        let status_sql: &str = match status {
            "all" => "",
            "active" => " AND (state != 'offline' AND last_ts >= ?)",
            "ended" => " AND NOT (state != 'offline' AND last_ts >= ?)",
            "errored" => " AND errors > 0",
            _ => return None,
        };
        let order_sql: &str = match sort {
            "recent" => "last_ts DESC",
            "tokens" => "total_tokens DESC",
            "calls" => "calls DESC",
            "duration" => "duration_ms DESC",
            _ => return None,
        };
        // 会话导出与 session_page 同一套单选语义（#9b 改造适配：单值包集合）
        let one = |v: Option<&str>| v.map(|s| vec![s.to_string()]);
        let f = self.report_filter(
            range,
            one(agent).as_deref(),
            one(project).as_deref(),
            one(model).as_deref(),
            None,
        )?;
        let conn = self.lock_conn();
        let (where_sql, mut params) = filter_where(&f);
        // 内层子查询按会话聚合；first_ts/last_ts 保持整数毫秒——状态档要拿它和
        // ENDED_AFTER_MS 阈值做数值比较（若在此处格式化成文本，SQLite 类型序
        // TEXT>INTEGER 会让"空闲超时"判断永远为真，ended 档全空——单测逮住过）
        let inner = format!(
            "SELECT u.session_id, COALESCE(s.agent, u.agent) AS agent,
                    COALESCE(s.state,'offline') AS state, s.model,
                    COALESCE(s.project_dir,'') AS project_dir, COALESCE(s.title,'') AS title,
                    MIN(u.ts) AS first_ts, MAX(u.ts) AS last_ts,
                    COALESCE(SUM(NOT u.is_background),0) AS calls,
                    COALESCE(SUM(COALESCE(u.input_tokens,0)),0) AS input_tokens,
                    COALESCE(SUM(COALESCE(u.output_tokens,0)),0) AS output_tokens,
                    COALESCE(SUM(COALESCE(u.cache_read_tokens,0)),0) AS cache_read_tokens,
                    COALESCE(SUM(COALESCE(u.cache_creation_tokens,0)),0) AS cache_creation_tokens,
                    COALESCE(SUM(COALESCE(u.reasoning_tokens,0)),0) AS reasoning_tokens,
                    COALESCE(SUM({TOTAL_EXPR}),0) AS total_tokens,
                    SUM(u.duration_ms) AS duration_ms,
                    CAST(AVG(u.ttft_ms) AS INTEGER) AS ttft_avg,
                    COALESCE(SUM(u.error_type IS NOT NULL),0) AS errors,
                    GROUP_CONCAT(DISTINCT u.error_type) AS error_types
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}
             GROUP BY u.session_id"
        );
        let kw = keyword.trim();
        if !kw.is_empty() {
            let pat = format!("%{}%", escape_like(kw));
            params.push(pat.clone().into());
            params.push(pat.into());
        }
        if status == "active" || status == "ended" {
            params.push((now_ms() - ENDED_AFTER_MS).into());
        }
        let kw_sql = if kw.is_empty() {
            ""
        } else {
            " AND (title LIKE ? ESCAPE '\\' OR project_dir LIKE ? ESCAPE '\\')"
        };
        // 外层：时间列才格式化为本机时区文本；时长/首字/错误类型补默认值，单元格免判空
        let sql = format!(
            "SELECT session_id, agent, state, model, project_dir, title,
                    datetime(first_ts/1000,'unixepoch','localtime') AS first_local,
                    datetime(last_ts/1000,'unixepoch','localtime') AS last_local,
                    calls, input_tokens, output_tokens, reasoning_tokens, cache_read_tokens,
                    cache_creation_tokens, total_tokens,
                    COALESCE(duration_ms,0) AS duration_ms,
                    COALESCE(CAST(ttft_avg AS TEXT),'') AS ttft, errors,
                    COALESCE(error_types,'') AS error_types
             FROM ({inner}) WHERE 1=1{kw_sql}{status_sql} ORDER BY {order_sql}"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return Some(String::new());
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            // 文本列当场转义，数字列直接转字符串（无逗号/引号无需转义）
            Ok(vec![
                csv_cell(&r.get::<_, String>("session_id")?),
                csv_cell(&r.get::<_, String>("agent")?),
                csv_cell(&r.get::<_, String>("state")?),
                csv_cell(&r.get::<_, Option<String>>("model")?.unwrap_or_default()),
                csv_cell(&r.get::<_, String>("project_dir")?),
                csv_cell(&r.get::<_, String>("title")?),
                csv_cell(&r.get::<_, String>("first_local")?),
                csv_cell(&r.get::<_, String>("last_local")?),
                r.get::<_, i64>("calls")?.to_string(),
                r.get::<_, i64>("input_tokens")?.to_string(),
                r.get::<_, i64>("output_tokens")?.to_string(),
                r.get::<_, i64>("reasoning_tokens")?.to_string(),
                r.get::<_, i64>("cache_read_tokens")?.to_string(),
                r.get::<_, i64>("cache_creation_tokens")?.to_string(),
                r.get::<_, i64>("total_tokens")?.to_string(),
                r.get::<_, i64>("duration_ms")?.to_string(),
                csv_cell(&r.get::<_, String>("ttft")?),
                r.get::<_, i64>("errors")?.to_string(),
                csv_cell(&r.get::<_, String>("error_types")?),
            ])
        });
        let mut out = String::from("\u{feff}会话ID,Agent,状态,模型,项目,标题,首次调用,最近调用,调用次数,输入,输出,思考,缓存读,缓存写,总Token,生成时长(毫秒),平均首字(毫秒),出错次数,错误类型\r\n");
        if let Ok(it) = rows {
            for cells in it.filter_map(|x| x.ok()) {
                out.push_str(&cells.join(","));
                out.push_str("\r\n");
            }
        }
        Some(out)
    }
}
