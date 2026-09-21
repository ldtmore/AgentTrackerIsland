//! ZCode 适配器：只读 `~\.zcode\cli\db\db.sqlite` 的 model_usage/session 表。
//! 勘察结论见 docs/01-RESEARCH.md §1：数据齐全，无需 hooks，纯只读零侵入。
//! 时间戳均为 Unix 毫秒，与自库约定一致，零转换。

use std::path::PathBuf;

use rusqlite::{OpenFlags, Connection};

use super::{AgentAdapter, CollectOutput, SessionInfo, provider_from_model};
use crate::store::UsageRow;

/// ZCode 数据库默认路径解析（%USERPROFILE%\.zcode\cli\db\db.sqlite）
fn default_db_path() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")?;
    let mut p = PathBuf::from(home);
    p.push(".zcode");
    p.push("cli");
    p.push("db");
    p.push("db.sqlite");
    Some(p)
}

pub struct ZcodeAdapter {
    db_path: PathBuf,
}

impl ZcodeAdapter {
    /// 使用默认路径构造；库不存在时 scan/collect 返回空（ZCode 未安装场景）
    pub fn new() -> Self {
        Self {
            db_path: default_db_path().unwrap_or_else(|| PathBuf::from("")),
        }
    }

    /// 只读打开（WAL 并发读安全；ZCode 运行与否均可读）
    fn open(&self) -> anyhow::Result<Connection> {
        if !self.db_path.exists() {
            anyhow::bail!("ZCode 数据库不存在：{}", self.db_path.display());
        }
        let conn = Connection::open_with_flags(
            &self.db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(conn)
    }
}

impl Default for ZcodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentAdapter for ZcodeAdapter {
    fn id(&self) -> &'static str {
        "zcode"
    }

    /// 扫描最近 90 天有活动、且有过模型调用的会话（纯观测会话无意义）
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        let conn = match self.open() {
            Ok(c) => c,
            Err(e) => {
                // 库不存在 = ZCode 未装（预期降级，静默）；其余打开失败 debug 留痕
                if self.db_path.exists() {
                    log::debug!("[zcode] 库打开失败（本轮按空处理）：{e:#}");
                }
                return Ok(vec![]);
            }
        };
        let cutoff = now_ms() - 90 * 24 * 3600 * 1000;
        let mut stmt = conn.prepare(
            "SELECT s.id, s.directory, s.title, s.time_created, s.time_updated,
                    (SELECT mu.model_id FROM model_usage mu
                      WHERE mu.session_id = s.id ORDER BY mu.started_at DESC LIMIT 1),
                    (SELECT MAX(mu.started_at) FROM model_usage mu WHERE mu.session_id = s.id)
             FROM session s
             WHERE s.time_updated > ?1
               AND EXISTS (SELECT 1 FROM model_usage mu WHERE mu.session_id = s.id)
             ORDER BY s.time_updated DESC
             LIMIT 100",
        )?;
        let rows = stmt.query_map([cutoff], |r| {
            let sid: String = r.get(0)?;
            let model: Option<String> = r.get(5)?;
            Ok(SessionInfo {
                id: format!("zcode:{sid}"),
                agent: "zcode".into(),
                provider: model.as_deref().and_then(provider_from_model),
                model,
                project_dir: r.get::<_, Option<String>>(1)?,
                title: r.get::<_, Option<String>>(2)?,
                first_seen_at: r.get::<_, i64>(3)?,
                last_seen_at: r.get::<_, i64>(4)?,
                last_usage_at: r.get::<_, Option<i64>>(6)?,
            })
        })?;
        // 行解析失败计数（Schema 漂移容忍，但持续失败需留痕排障）
        let mut err_rows = 0usize;
        let out = rows
            .filter_map(|x| match x {
                Ok(v) => Some(v),
                Err(_) => {
                    err_rows += 1;
                    None
                }
            })
            .collect();
        if err_rows > 0 {
            log::debug!("[zcode] 扫描 {err_rows} 行解析失败已跳过（Schema 漂移？）");
        }
        Ok(out)
    }

    /// 水位增量读取 model_usage（列白名单，未知列忽略以容忍 Schema 漂移）。
    /// source_id = 源库行 id（"mu:{id}"，2026-09-21 幂等键升级）；
    /// error_type：用户主动取消（cancelled_by_user=1）不算错误——取消是正常操作
    /// 而非故障，计入"出错次数/错误分布"会污染口径（所有者 2026-09-21 拍板）
    fn collect_usage(&self, watermark_ts: i64) -> anyhow::Result<CollectOutput> {
        let conn = match self.open() {
            Ok(c) => c,
            Err(e) => {
                // 库不存在 = ZCode 未装（预期降级，静默）；其余打开失败 debug 留痕
                if self.db_path.exists() {
                    log::debug!("[zcode] 库打开失败（本轮按空处理）：{e:#}");
                }
                return Ok(CollectOutput::default());
            }
        };
        let mut stmt = conn.prepare(
            "SELECT id, session_id, model_id, started_at,
                    input_tokens, output_tokens, reasoning_tokens,
                    cache_read_input_tokens, cache_creation_input_tokens,
                    duration_ms, time_to_first_token_ms,
                    CASE WHEN cancelled_by_user = 1 THEN NULL ELSE error_type END
             FROM model_usage
             WHERE started_at > ?1
             ORDER BY started_at ASC
             LIMIT 5000",
        )?;
        let rows = stmt.query_map([watermark_ts], |r| {
            // 源库 id 实测为 TEXT 型（uuid 类字符串），必须按 String 读
            let id: String = r.get(0)?;
            let session_id: String = r.get(1)?;
            let model: String = r.get(2)?;
            let provider = provider_from_model(&model);
            Ok(UsageRow {
                session_id: format!("zcode:{session_id}"),
                agent: "zcode".into(),
                model,
                provider,
                ts: r.get(3)?,
                input_tokens: r.get(4)?,
                output_tokens: r.get(5)?,
                reasoning_tokens: r.get(6)?,
                cache_read_tokens: r.get(7)?,
                cache_creation_tokens: r.get(8)?,
                duration_ms: r.get(9)?,
                ttft_ms: r.get(10)?,
                error_type: r.get(11)?,
                source_id: Some(format!("mu:{id}")),
                is_background: false,
            })
        })?;
        let mut err_rows = 0usize;
        let mut out = vec![];
        for x in rows {
            match x {
                Ok(v) => out.push(v),
                Err(_) => err_rows += 1,
            }
        }
        if err_rows > 0 {
            log::debug!("[zcode] 采集 {err_rows} 行解析失败已跳过（Schema 漂移？）");
        }
        Ok(CollectOutput { rows: out, cost_snapshots: vec![], titles: vec![] })
    }
}

/// 当前 Unix 毫秒
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 集成测试：连接本机真实 ZCode 库（无 ZCode 环境跳过）
    /// 手动运行：cargo test -- --ignored
    #[test]
    #[ignore]
    fn test_real_zcode_collect() {
        let ad = ZcodeAdapter::new();
        let sessions = ad.scan_sessions().unwrap();
        assert!(!sessions.is_empty(), "本机应有活跃 ZCode 会话");
        assert!(sessions[0].id.starts_with("zcode:sess_"));
        assert!(sessions[0].last_usage_at.is_some());

        let usage = ad.collect_usage(0).unwrap().rows;
        assert!(!usage.is_empty(), "本机应有历史用量");
        for u in usage.iter().take(5) {
            assert!(u.ts > 1_700_000_000_000, "时间戳应为毫秒：{}", u.ts);
            assert_eq!(u.agent, "zcode");
            assert!(u.model.to_ascii_lowercase().starts_with("glm"), "当前主力模型：{}", u.model);
        }
        // 水位增量：用最大 ts 再采一次，应无新行
        let max_ts = usage.iter().map(|u| u.ts).max().unwrap();
        assert!(ad.collect_usage(max_ts).unwrap().rows.is_empty());
    }

    /// 幂等入自库：同批数据插两遍，第二遍 0 行
    #[test]
    #[ignore]
    fn test_real_zcode_into_store_idempotent() {
        let ad = ZcodeAdapter::new();
        let usage = ad.collect_usage(0).unwrap().rows;
        let mut db = std::env::temp_dir();
        db.push(format!("at-t3-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let store = crate::store::Store::open(&db).unwrap();
        let n1 = store.insert_usage(&usage);
        assert_eq!(n1, usage.len());
        let n2 = store.insert_usage(&usage);
        assert_eq!(n2, 0, "重复插入应全部被幂等键忽略");
        let _ = std::fs::remove_file(&db);
    }
}
