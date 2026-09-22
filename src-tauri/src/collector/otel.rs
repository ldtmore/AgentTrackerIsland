//! OtelSink 骨架（M2-12）：Gemini CLI / Qwen Code 的 OTel outfile 增强通道。
//! 只做 outfile 轮询解析，OTLP gRPC receiver 留 backlog（04-总纲 §2.3.4）。
//! 调研依据：01-RESEARCH §13.4（两家 telemetry 源码级核实，2026-09-23）。
//!
//! 落盘形态（源码确证）：
//! - CLI 侧 `safeJsonStringify(data, 2) + '\n'` 追加写——**pretty 多行 JSON 值流**（非
//!   单行 JSONL），span/log/metric 三类记录混写同一文件；本模块用 serde_json 的
//!   StreamDeserializer 按值括号配平解析，精确字节游标推进，截断的半条留在文件里
//!   待下轮补读（不能复用引擎 64KB 回退的 IncrementalFileReader——重读会重复产出）。
//! - log 记录的 JSON 只有 resource / instrumentationScope / attributes 三个可枚举键
//!   （OTel sdk-logs 0.218.0 的 LogRecordImpl：时间戳/body 全是私有字段不落盘）——
//!   解析只依赖 attributes，时间取 `attributes["event.timestamp"]`（两家自带）。
//! - Gemini 每次 API 响应写**两条** log（api_response 全量计数＋semantic 摘要仅 2 项），
//!   必须按 event.name 白名单过滤防双计；Qwen 为**单条**记录（事件顶层展开进 attributes）。
//!
//! 隐私红线（04-总纲 §2.8.2）：logPrompts 默认 true，outfile 的 attributes 可能携带
//! prompt/response 全文（response_text / request_text / gen_ai.*.messages）——本模块
//! 白名单取数，只提取数字计数/模型/会话 id/时间戳/错误类型，文本内容一律不碰不记。
//!
//! 通道裁定（所有者拍板 2026-09-23）：api_response 的 token 行**解析与对账就绪但暂不
//! 入库**——与转录通道是同一回合的两份记录且无公共 id 可对齐，入库必双计；api_error
//! 行直接走 recent_error 链路（转录/hooks 均没有的精确信号，token 全 None 无冲突）。
//! 装机对账后若切 outfile 为主通道，适配器侧把 `batch.rows` 一并并入即可（一行改动）。

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use super::engine::{iso_to_ms, mtime_ms, HotSignal};
use super::provider_from_model;
use crate::store::UsageRow;

/// 两家差异的静态声明（提取逻辑零 per-agent 分支，差异全收敛在此表）
pub struct OtelProfile {
    /// agent 标识（会话命名空间前缀/UsageRow.agent，与转录适配器同字串——
    /// 同一会话的用量/错误无论来自哪个通道都归到同一命名空间）
    pub agent: &'static str,
    /// api_response 事件的 event.name 精确值
    pub response_event: &'static str,
    /// api_error 事件的 event.name 精确值
    pub error_event: &'static str,
    /// 幂等键前缀（ote=gemini / otq=qwen，与 gm:/qw: 风格一致）
    pub source_prefix: &'static str,
    /// telemetry.enabled 的 env 覆盖变量名
    pub enabled_env: &'static str,
    /// telemetry.outfile 的 env 覆盖变量名
    pub outfile_env: &'static str,
}

/// Gemini CLI（google-gemini/gemini-cli v0.60.0，2026-09-23 源码核实）
pub const GEMINI_PROFILE: OtelProfile = OtelProfile {
    agent: "gemini",
    response_event: "gemini_cli.api_response",
    error_event: "gemini_cli.api_error",
    source_prefix: "ote",
    enabled_env: "GEMINI_TELEMETRY_ENABLED",
    outfile_env: "GEMINI_TELEMETRY_OUTFILE",
};

/// Qwen Code（QwenLM/qwen-code v0.24.4，2026-09-23 源码核实）
pub const QWEN_PROFILE: OtelProfile = OtelProfile {
    agent: "qwen-code",
    response_event: "api_response",
    error_event: "api_error",
    source_prefix: "otq",
    enabled_env: "QWEN_TELEMETRY_ENABLED",
    outfile_env: "QWEN_TELEMETRY_OUTFILE",
};

/// 一轮 outfile 消费的产出：
/// rows = api_response 的 token 行（骨架期仅对账测试用，不入库）；
/// errors = api_error 错误信号行（适配器并入 CollectOutput 走 recent_error）
#[derive(Debug, Default)]
pub struct OtelBatch {
    pub rows: Vec<UsageRow>,
    pub errors: Vec<UsageRow>,
}

/// 发现缓存：settings.json mtime → outfile 路径（含「未启用」的负结果——
/// 每轮一次 stat，settings 变了才重读解析）
#[derive(Default)]
struct DiscoveryCache {
    settings_mtime: Option<i64>,
    outfile: Option<PathBuf>,
}

/// 流游标：(outfile mtime, 已消费字节偏移, 连续卡点计数)
/// 偏移语义＝下一轮从此字节起读；截断半条回退到值起点等补全
#[derive(Default)]
struct StreamCursor {
    mtime: i64,
    offset: u64,
    stuck: u32,
}

/// 中途坏数据的防卡死阈值：同点连续失败 N 次后跳到下一个换行放弃坏段
/// （正常截断发生在文件尾，不占此计数——见 collect 的判定）
const STUCK_SKIP_THRESHOLD: u32 = 3;

pub struct OtelOutfileSink {
    inner: Arc<Inner>,
}

struct Inner {
    profile: OtelProfile,
    /// 用户 settings.json 路径（telemetry 配置载体；JSONC 注释容忍）
    settings_path: PathBuf,
    discovery: Mutex<DiscoveryCache>,
    cursor: Mutex<StreamCursor>,
}

impl OtelOutfileSink {
    /// 按各家 profile 构造（settings_path 由适配器给定，支持单测注入临时目录）
    pub fn new(profile: OtelProfile, settings_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                profile,
                settings_path,
                discovery: Mutex::new(DiscoveryCache::default()),
                cursor: Mutex::new(StreamCursor::default()),
            }),
        }
    }

    /// 快轮信号：outfile 存在即活动（未配置/未创建返回 None，配置后从无到有即变化）
    pub fn hot_signal(&self) -> HotSignal {
        let inner = self.inner.clone();
        HotSignal::File(Arc::new(move || inner.discover()))
    }

    /// 消费 outfile 自上次游标以来的新记录（内部全程容错，任何失败只影响当轮产出，
    /// 静默降级——红线④：OTel 未配置/失败走启发式兜底）
    pub fn collect(&self) -> OtelBatch {
        let mut batch = OtelBatch::default();
        // 未配置：清游标（防改配置指向新文件时残留旧偏移），静默返回
        let Some(path) = self.inner.discover() else {
            *self.inner.lock_cursor() = StreamCursor::default();
            return batch;
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            return batch; // 尚未创建（典型：enabled 但 CLI 未启动），静默
        };
        let len = meta.len();
        let mtime = mtime_ms(&path);
        let mut cur = self.inner.lock_cursor();
        if cur.mtime == mtime {
            return batch; // 无新字节
        }
        if cur.offset >= len {
            cur.offset = 0; // 文件重建/轮转变小或等长重写：归零全读
            cur.stuck = 0;
        }
        let Ok(mut f) = std::fs::File::open(&path) else {
            return batch; // 打不开（被占用等），下轮重试
        };
        if cur.offset > 0 {
            if f.seek(SeekFrom::Start(cur.offset)).is_err() {
                return batch;
            }
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return batch;
        }
        let base = cur.offset;
        // 尾部多字节 UTF-8 字符可能被截断（prompt 含中文时常见）：只解析合法前缀，
        // 残缺字节留在文件里下轮补读——不能 from_utf8_lossy（U+FFFD 替换会让
        // 字节偏移对不上真实文件位置）
        let valid = match std::str::from_utf8(&buf) {
            Ok(_) => buf.len(),
            Err(e) => e.valid_up_to(),
        };
        let text = match std::str::from_utf8(&buf[..valid]) {
            Ok(t) => t,
            Err(_) => return batch,
        };
        // pretty JSON 值流按值配平解析；last_good＝上一成功值结束后的段内字节偏移
        // （截断回退点：值可能写一半，从值起点整条重读）
        let mut de = serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>();
        let mut last_good = 0usize;
        loop {
            match de.next() {
                Some(Ok(v)) => {
                    self.inner.extract(&v, &mut batch);
                    last_good = de.byte_offset();
                }
                Some(Err(_)) => {
                    let err_local = de.byte_offset();
                    let err_at = base + err_local as u64;
                    if err_at >= len.saturating_sub(8) {
                        // 文件尾附近失败＝记录正在写：停在值起点等下轮补全，不计卡点
                        cur.offset = base + last_good as u64;
                        cur.stuck = 0;
                    } else if cur.stuck + 1 >= STUCK_SKIP_THRESHOLD {
                        // 文件中段坏数据（磁盘异常/手动编辑等）：从失败点向后跳一行放行，
                        // 保后续记录可读（坏段本身永久放弃）。⚠️ 起点必须用失败点而非
                        // last_good——值结束处紧跟的换行属值间空白，从那里跳只会空转
                        let skip = text[err_local..]
                            .find('\n')
                            .map(|i| err_local + i + 1)
                            .unwrap_or(valid);
                        cur.offset = base + skip as u64;
                        cur.stuck = 0;
                    } else {
                        cur.stuck += 1;
                        cur.offset = base + last_good as u64;
                    }
                    break;
                }
                None => {
                    // 流干净走完（byte_offset ≤ 合法前缀长度，尾部残缺字节自动留下轮）
                    cur.offset = base + de.byte_offset() as u64;
                    cur.stuck = 0;
                    break;
                }
            }
        }
        cur.mtime = mtime;
        batch
    }
}

impl Inner {
    fn lock_cursor(&self) -> MutexGuard<'_, StreamCursor> {
        self.cursor.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_discovery(&self) -> MutexGuard<'_, DiscoveryCache> {
        self.discovery.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// outfile 路径发现：env 覆盖 > settings.json `telemetry.outfile`，
    /// 且必须 `telemetry.enabled`（env 或 settings 任一为 true）——与 CLI 侧
    /// initializeTelemetry 的硬前提一致（enabled=false 时整个 SDK 不初始化不落盘）。
    /// 带缓存：settings mtime 未变直接走缓存（快轮每 1~2s 调用一次，不能每次读盘）
    fn discover(&self) -> Option<PathBuf> {
        let mtime = mtime_ms(&self.settings_path);
        let mut cache = self.lock_discovery();
        if mtime > 0 && cache.settings_mtime == Some(mtime) {
            return cache.outfile.clone();
        }
        let outfile = discover_from(&self.settings_path, &self.profile);
        cache.settings_mtime = Some(mtime);
        cache.outfile = outfile.clone();
        outfile
    }

    /// 从一条 outfile 记录提取 UsageRow（白名单 event.name；其余 span/metric/
    /// semantic/工具事件一律跳过——Gemini semantic 摘要记录只带 2 项计数，误取即双计）
    fn extract(&self, v: &serde_json::Value, batch: &mut OtelBatch) {
        // 只认 log 记录形态：attributes 为对象（metrics 记录无此顶层键自然跳过；
        // span 有 attributes 但 event.name 不在白名单）
        let Some(attrs) = v.get("attributes").and_then(|a| a.as_object()) else {
            return;
        };
        let Some(ev) = attrs.get("event.name").and_then(|e| e.as_str()) else {
            return;
        };
        let p = &self.profile;
        if ev == p.response_event {
            if let Some(row) = token_row(attrs, p) {
                batch.rows.push(row);
            }
        } else if ev == p.error_event {
            if let Some(row) = error_row(attrs, p) {
                batch.errors.push(row);
            }
        }
    }
}

/// 路径发现（无缓存版本，单测直接验证发现链）：
/// enabled = env 覆盖为 "true" 或 settings.telemetry.enabled == true；
/// outfile = env 覆盖值 或 settings.telemetry.outfile（`~/` 前缀展开到用户目录）
fn discover_from(settings_path: &Path, profile: &OtelProfile) -> Option<PathBuf> {
    let settings = std::fs::read_to_string(settings_path)
        .ok()
        .and_then(|raw| {
            // Gemini settings.json 支持 JSONC 注释（CLI 侧 stripJsonComments 同款宽容）：
            // 先按纯 JSON 试，失败再去注释重试（绝大多数用户配置无注释，零成本快路径）
            serde_json::from_str::<serde_json::Value>(&raw)
                .or_else(|_| serde_json::from_str::<serde_json::Value>(&strip_json_comments(&raw)))
                .ok()
        });
    let env_true = |name: &str| {
        std::env::var_os(name).is_some_and(|v| v == std::ffi::OsStr::new("true"))
    };
    let enabled = env_true(profile.enabled_env)
        || settings
            .as_ref()
            .and_then(|s| s.get("telemetry")?.get("enabled")?.as_bool())
            .unwrap_or(false);
    if !enabled {
        return None;
    }
    let raw = std::env::var_os(profile.outfile_env)
        .map(PathBuf::from)
        .or_else(|| {
            settings
                .as_ref()
                .and_then(|s| s.get("telemetry")?.get("outfile")?.as_str())
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })?;
    expand_tilde(&raw).into()
}

/// `~/` 或 `~\` 前缀展开到 %USERPROFILE%（CLI 侧是否展开属装机核实项；
/// 我们两侧都支持，无法展开时原样返回）
fn expand_tilde(p: &Path) -> PathBuf {
    let Some(s) = p.to_str() else { return p.to_path_buf() };
    if s == "~" {
        if let Some(home) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(home);
        }
        return p.to_path_buf();
    }
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        if let Some(home) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(home).join(rest);
        }
    }
    p.to_path_buf()
}

/// 简易 JSONC 去注释（字符串字面量内的 // 与 /* 不受影响）；只处理注释不处理尾逗号
/// （CLI 侧 stripJsonComments 同款语义），解析仍交给 serde
fn strip_json_comments(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    let mut in_string = false;
    let mut prev_escape = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            // \" 转义内不结束字符串（prev_escape 只在紧邻时成立）
            if c == '"' && !prev_escape {
                in_string = false;
            }
            prev_escape = c == '\\' && !prev_escape;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                prev_escape = false;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                while let Some(n) = chars.next() {
                    if n == '\n' {
                        out.push('\n'); // 保留换行维持行号可读
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next(); // 吃掉 '*'
                while let Some(n) = chars.next() {
                    if n == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// 从 attributes 取整数（缺失/类型漂移按缺省处理——格式漂移容忍）
fn num(attrs: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<i64> {
    attrs.get(key).and_then(|v| v.as_i64())
}

/// api_response → token 行。
/// token 字段两家同名同位置（attributes 顶层，源码确证）：input/output/cached/thoughts
/// ＋Gemini 独有 tool/total；tool 是 prompt 的口径说明项、total 是求和项，均不入账
/// （与转录适配器同口径：官方 /stats 的 input = prompt − cached，cached ⊆ prompt）。
/// response_id 仅 Qwen 有，作幂等键；缺失（Gemini）按会话＋毫秒时间戳兜底。
fn token_row(
    attrs: &serde_json::Map<String, serde_json::Value>,
    p: &OtelProfile,
) -> Option<UsageRow> {
    let session = attrs.get("session.id")?.as_str()?;
    let ts = attrs.get("event.timestamp")?.as_str().and_then(iso_to_ms)?;
    let model = attrs.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let g = |k: &str| attrs.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let prompt = g("input_token_count");
    let cached = g("cached_content_token_count");
    // cached ⊆ prompt（OpenAI 语义归一化后两家同）：入库前拆分；倒挂视为分立语义照抄
    let (input, cache_read) =
        if prompt >= cached { (prompt - cached, cached) } else { (prompt, cached) };
    let source = match attrs.get("response_id").and_then(|r| r.as_str()) {
        Some(rid) if !rid.is_empty() => format!("{}:{}", p.source_prefix, rid),
        _ => format!("{}:{}:{}", p.source_prefix, session, ts),
    };
    Some(UsageRow {
        session_id: format!("{}:{}", p.agent, session),
        agent: p.agent.into(),
        model: model.to_string(),
        provider: provider_from_model(model),
        ts,
        input_tokens: Some(input),
        output_tokens: Some(g("output_token_count")),
        reasoning_tokens: Some(g("thoughts_token_count")),
        cache_read_tokens: Some(cache_read),
        cache_creation_tokens: Some(0),
        duration_ms: num(attrs, "duration_ms"),
        ttft_ms: num(attrs, "ttft_ms"),
        error_type: None,
        source_id: Some(source),
        is_background: false,
    })
}

/// api_error → 错误信号行（token 全 None，走 recent_error 链路；错误类型文本截 120，
/// 与转录适配器 error 行同构）。错误文本字段宽容序列两家通吃：
/// Qwen 顶层展开 error_type/error_message；Gemini 为 error.type/error.message/error。
fn error_row(
    attrs: &serde_json::Map<String, serde_json::Value>,
    p: &OtelProfile,
) -> Option<UsageRow> {
    let session = attrs.get("session.id")?.as_str()?;
    let ts = attrs.get("event.timestamp")?.as_str().and_then(iso_to_ms)?;
    let text = ["error_type", "error.type", "error_message", "error.message", "error"]
        .iter()
        .find_map(|k| attrs.get(*k).and_then(|v| v.as_str()))
        .unwrap_or("unknown");
    let model = attrs.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let source = match attrs.get("response_id").and_then(|r| r.as_str()) {
        Some(rid) if !rid.is_empty() => format!("{}:e:{}", p.source_prefix, rid),
        _ => format!("{}:e:{}:{}", p.source_prefix, session, ts),
    };
    Some(UsageRow {
        session_id: format!("{}:{}", p.agent, session),
        agent: p.agent.into(),
        model: model.to_string(),
        provider: None,
        ts,
        input_tokens: None,
        output_tokens: None,
        reasoning_tokens: None,
        cache_read_tokens: None,
        cache_creation_tokens: None,
        duration_ms: num(attrs, "duration_ms"),
        ttft_ms: None,
        error_type: Some(text.chars().take(120).collect()),
        source_id: Some(source),
        is_background: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("at-otel-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 写一条 OTel log 记录的 pretty JSON（模拟 safeJsonStringify(data,2)+'\n'：
    /// 只有 resource/instrumentationScope/attributes 三个可枚举键，无顶层时间戳）
    fn log_record(event_name: &str, attrs: serde_json::Value) -> String {
        let mut all = serde_json::json!({
            "resource": {"attributes": {"service.name": "gemini-cli"}},
            "instrumentationScope": {"name": "gemini-cli"},
            "attributes": attrs,
        });
        all["attributes"]["event.name"] = serde_json::json!(event_name);
        let mut s = serde_json::to_string_pretty(&all).unwrap();
        s.push('\n');
        s
    }

    /// 一条 Gemini api_response 记录的标准 attributes（6 项计数＋session/model/时间）
    fn gemini_response_attrs(ts: &str, input: i64, output: i64, cached: i64) -> serde_json::Value {
        serde_json::json!({
            "session.id": "sess-1",
            "event.timestamp": ts,
            "model": "gemini-2.5-pro",
            "duration_ms": 1234,
            "input_token_count": input,
            "output_token_count": output,
            "cached_content_token_count": cached,
            "thoughts_token_count": 30,
            "tool_token_count": 0,
            "total_token_count": input + output,
            "prompt_id": "p1",
            "status_code": 200,
            // 隐私陷阱字段（logPrompts=true 时真实存在）：必须被白名单忽略
            "response_text": "这段回复全文绝不能进自库",
        })
    }

    fn ts_ms(s: &str) -> i64 {
        iso_to_ms(s).unwrap()
    }

    /// pretty 流解析＋白名单过滤：api_response 产出；semantic 摘要（2 项重复计数）、
    /// metrics（无 attributes）、span（event.name 不在白名单）全部跳过——防双计核心
    #[test]
    fn test_stream_parse_and_filter() {
        let dir = tmp_dir("stream");
        let outfile = dir.join("telemetry.log");
        let mut content = String::new();
        content.push_str(&log_record(
            "gemini_cli.api_response",
            gemini_response_attrs("2026-09-23T10:00:01.000Z", 900, 150, 200),
        ));
        // Gemini 双记录的第二条：semantic 摘要（gen_ai.usage 只有 2 项）——必须跳过
        content.push_str(&log_record(
            "gen_ai.client.inference.operation.details",
            serde_json::json!({
                "session.id": "sess-1",
                "event.timestamp": "2026-09-23T10:00:01.000Z",
                "model": "gemini-2.5-pro",
                "gen_ai.usage.input_tokens": 900,
                "gen_ai.usage.output_tokens": 150,
            }),
        ));
        // metrics 记录（每 10s 累计导出，无 attributes 顶层键）——跳过
        content.push_str(
            "{\"resource\":{\"attributes\":{}},\"scopeMetrics\":[{\"metrics\":[{\"name\":\
             \"gemini_cli.token.usage\"}]}]}\n",
        );
        // span 记录（有 attributes 但 event.name 非白名单）——跳过
        content.push_str(&log_record(
            "the_user_prompt",
            serde_json::json!({"session.id": "sess-1", "input_token_count": 999}),
        ));
        std::fs::write(&outfile, &content).unwrap();

        let settings = dir.join("settings.json");
        let settings_text = format!(
            r#"{{"telemetry": {{"enabled": true, "outfile": {}}}}}"#,
            serde_json::to_string(outfile.to_str().unwrap()).unwrap()
        );
        std::fs::write(&settings, settings_text).unwrap();

        let sink = OtelOutfileSink::new(GEMINI_PROFILE, settings.clone());
        let batch = sink.collect();
        assert_eq!(batch.rows.len(), 1, "只应产出 api_response 一条：{:?}", batch.rows);
        assert!(batch.errors.is_empty());
        let r = &batch.rows[0];
        // cached ⊆ prompt 拆分：900 − 200 = 700
        assert_eq!(r.input_tokens, Some(700));
        assert_eq!(r.cache_read_tokens, Some(200));
        assert_eq!(r.output_tokens, Some(150));
        assert_eq!(r.reasoning_tokens, Some(30));
        assert_eq!(r.session_id, "gemini:sess-1");
        assert_eq!(r.model, "gemini-2.5-pro");
        assert_eq!(r.provider.as_deref(), Some("google"));
        assert_eq!(r.ts, ts_ms("2026-09-23T10:00:01.000Z"));
        assert_eq!(r.duration_ms, Some(1234));

        // mtime 未变再采：零产出零重复
        let batch2 = sink.collect();
        assert!(batch2.rows.is_empty() && batch2.errors.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 截断续读：末条写一半时本轮不产出、游标停在值起点；补全后整条产出且无重复
    #[test]
    fn test_truncated_tail_resume() {
        let dir = tmp_dir("trunc");
        let outfile = dir.join("telemetry.log");
        let full = log_record(
            "api_response",
            serde_json::json!({
                "session.id": "q-sess",
                "event.timestamp": "2026-09-23T10:00:02.000Z",
                "model": "qwen3-coder-plus",
                "input_token_count": 100,
                "output_token_count": 20,
                "cached_content_token_count": 0,
                "thoughts_token_count": 0,
                "response_id": "resp-abc",
                "ttft_ms": 350,
            }),
        );
        let half = &full[..full.len() / 2];
        std::fs::write(&outfile, half).unwrap();

        let settings = dir.join("settings.json");
        let settings_text = format!(
            r#"{{"telemetry": {{"enabled": true, "outfile": {}}}}}"#,
            serde_json::to_string(outfile.to_str().unwrap()).unwrap()
        );
        std::fs::write(&settings, settings_text).unwrap();
        let sink = OtelOutfileSink::new(QWEN_PROFILE, settings.clone());

        let b1 = sink.collect();
        assert!(b1.rows.is_empty(), "半条不应产出");

        // CLI 写完余下半条：整条补读产出
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(&outfile, &full).unwrap();
        let b2 = sink.collect();
        assert_eq!(b2.rows.len(), 1, "补全后应整条产出");
        assert_eq!(b2.rows[0].source_id.as_deref(), Some("otq:resp-abc"), "Qwen 用 response_id 幂等");
        assert_eq!(b2.rows[0].ttft_ms, Some(350));
        assert_eq!(b2.rows[0].session_id, "qwen-code:q-sess");

        // 追加第二条：只出新增，游标不回退重读
        std::thread::sleep(std::time::Duration::from_millis(30));
        let second = log_record(
            "api_response",
            serde_json::json!({
                "session.id": "q-sess",
                "event.timestamp": "2026-09-23T10:00:03.000Z",
                "model": "qwen3-coder-plus",
                "input_token_count": 50,
                "output_token_count": 5,
                "cached_content_token_count": 10,
            }),
        );
        std::fs::write(&outfile, format!("{full}{second}")).unwrap();
        let b3 = sink.collect();
        assert_eq!(b3.rows.len(), 1, "只出新增一条，实际 {:?}", b3.rows.len());
        // response_id 缺失（此条模拟 Qwen 异常场景）：回落 会话＋毫秒 兜底幂等键
        let expect = format!("otq:q-sess:{}", ts_ms("2026-09-23T10:00:03.000Z"));
        assert_eq!(b3.rows[0].source_id.as_deref(), Some(expect.as_str()));
        assert_eq!(b3.rows[0].input_tokens, Some(40), "50 − 10 拆分");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 中段坏数据防卡死：同点连续失败 3 次后跳一行放行，后续新记录可继续读出
    #[test]
    fn test_midfile_corruption_recovers() {
        let dir = tmp_dir("corrupt");
        let outfile = dir.join("telemetry.log");
        let good1 = log_record(
            "gemini_cli.api_response",
            serde_json::json!({
                "session.id": "s",
                "event.timestamp": "2026-09-23T10:00:01.000Z",
                "model": "gemini-2.5-pro",
                "input_token_count": 10,
                "output_token_count": 1,
                "cached_content_token_count": 0,
            }),
        );
        // 中段坏数据（非合法 JSON 且带换行边界）
        let bad = "this is {broken telemetry\n";
        std::fs::write(&outfile, format!("{good1}{bad}")).unwrap();
        let settings = dir.join("settings.json");
        let settings_text = format!(
            r#"{{"telemetry": {{"enabled": true, "outfile": {}}}}}"#,
            serde_json::to_string(outfile.to_str().unwrap()).unwrap()
        );
        std::fs::write(&settings, settings_text).unwrap();
        let sink = OtelOutfileSink::new(GEMINI_PROFILE, settings.clone());

        let b1 = sink.collect();
        assert_eq!(b1.rows.len(), 1, "第一条完整记录应产出");
        // 坏数据之后追加新记录：连续 3 轮卡点后应跳行恢复
        // （注意：卡点轮游标停在坏段前，恢复轮只产出 good2——good1 首轮已消费）
        let good2 = log_record(
            "gemini_cli.api_response",
            serde_json::json!({
                "session.id": "s",
                "event.timestamp": "2026-09-23T10:00:02.000Z",
                "model": "gemini-2.5-pro",
                "input_token_count": 20,
                "output_token_count": 2,
                "cached_content_token_count": 0,
            }),
        );
        for round in 1..=5 {
            std::thread::sleep(std::time::Duration::from_millis(30));
            std::fs::write(&outfile, format!("{good1}{bad}{good2}")).unwrap();
            let b = sink.collect();
            let recovered = b.rows.iter().any(|r| r.input_tokens == Some(20));
            if recovered {
                assert!(round >= STUCK_SKIP_THRESHOLD, "不应早于阈值恢复");
                return;
            }
            assert!(b.rows.is_empty() || b.rows.iter().all(|r| r.input_tokens == Some(10)),
                "恢复前不应产出 good2（异常产出 {:?}）", b.rows.iter().map(|r| r.input_tokens).collect::<Vec<_>>());
        }
        panic!("坏段后未在防卡死阈值内恢复");
    }

    /// 文件重建（变小）归零全读：游标不残留旧偏移
    #[test]
    fn test_file_rebuild_resets_cursor() {
        let dir = tmp_dir("rebuild");
        let outfile = dir.join("telemetry.log");
        let rec = |ts: &str, n: i64| {
            log_record(
                "gemini_cli.api_response",
                serde_json::json!({
                    "session.id": "s",
                    "event.timestamp": ts,
                    "model": "gemini-2.5-pro",
                    "input_token_count": n,
                    "output_token_count": 1,
                    "cached_content_token_count": 0,
                }),
            )
        };
        std::fs::write(&outfile, rec("2026-09-23T10:00:01.000Z", 10)).unwrap();
        let settings = dir.join("settings.json");
        let settings_text = format!(
            r#"{{"telemetry": {{"enabled": true, "outfile": {}}}}}"#,
            serde_json::to_string(outfile.to_str().unwrap()).unwrap()
        );
        std::fs::write(&settings, settings_text).unwrap();
        let sink = OtelOutfileSink::new(GEMINI_PROFILE, settings);
        assert_eq!(sink.collect().rows.len(), 1);

        // 用户清空重建 outfile：归零重读
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(&outfile, rec("2026-09-23T10:00:05.000Z", 99)).unwrap();
        let b = sink.collect();
        assert_eq!(b.rows.len(), 1);
        assert_eq!(b.rows[0].input_tokens, Some(99));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 尾部多字节 UTF-8 截断（中文 prompt 场景）：本轮不出半条，补全后整条产出无重复
    #[test]
    fn test_utf8_tail_split() {
        let dir = tmp_dir("utf8");
        let outfile = dir.join("telemetry.log");
        // response_text 放长中文（纯为制造多字节截断点；解析必须忽略其内容）
        let full = log_record(
            "gemini_cli.api_response",
            serde_json::json!({
                "session.id": "s",
                "event.timestamp": "2026-09-23T10:00:01.000Z",
                "model": "gemini-2.5-pro",
                "input_token_count": 7,
                "output_token_count": 1,
                "cached_content_token_count": 0,
                "response_text": "这是一段很长的中文回复内容用于制造多字节字符的截断边界场景",
            }),
        );
        // 在字节层面切掉尾部若干字节（可能落在多字节字符中间）；
        // 切点非法时退一位再试（测试自身保证得到合法 UTF-8 前缀）
        let cut_str = match std::str::from_utf8(&full.as_bytes()[..full.len() - 3]) {
            Ok(s) => s,
            Err(_) => std::str::from_utf8(&full.as_bytes()[..full.len() - 4]).unwrap(),
        };
        std::fs::write(&outfile, cut_str).unwrap();

        let settings = dir.join("settings.json");
        let settings_text = format!(
            r#"{{"telemetry": {{"enabled": true, "outfile": {}}}}}"#,
            serde_json::to_string(outfile.to_str().unwrap()).unwrap()
        );
        std::fs::write(&settings, settings_text).unwrap();
        let sink = OtelOutfileSink::new(GEMINI_PROFILE, settings);

        let b1 = sink.collect();
        assert!(b1.rows.is_empty(), "UTF-8 残缺尾部不应产出半条");

        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(&outfile, &full).unwrap();
        let b2 = sink.collect();
        assert_eq!(b2.rows.len(), 1, "补全后整条产出");
        assert_eq!(b2.rows[0].input_tokens, Some(7));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// api_error 行提取：Gemini（error.type/error.message 带点键）与 Qwen
    /// （error_type/error_message 顶层展开）两形态通吃；token 全 None
    #[test]
    fn test_error_event_rows() {
        let dir = tmp_dir("err");
        let outfile = dir.join("telemetry.log");
        let mut content = String::new();
        content.push_str(&log_record(
            "gemini_cli.api_error",
            serde_json::json!({
                "session.id": "s1",
                "event.timestamp": "2026-09-23T10:00:01.000Z",
                "model": "gemini-2.5-pro",
                "error": "429 Too Many Requests",
                "error.type": "rate_limit",
                "status_code": 429,
                "duration_ms": 88,
            }),
        ));
        content.push_str(&log_record(
            "api_error",
            serde_json::json!({
                "session.id": "s2",
                "event.timestamp": "2026-09-23T10:00:02.000Z",
                "model": "qwen3-coder-plus",
                "response_id": "resp-e1",
                "error_message": "Request failed with status 500",
                "error_type": "server_error",
            }),
        ));
        std::fs::write(&outfile, &content).unwrap();
        let settings = dir.join("settings.json");
        let settings_text = format!(
            r#"{{"telemetry": {{"enabled": true, "outfile": {}}}}}"#,
            serde_json::to_string(outfile.to_str().unwrap()).unwrap()
        );
        std::fs::write(&settings, settings_text).unwrap();

        // 两家 profile 各自只取自家 event.name
        let g = OtelOutfileSink::new(GEMINI_PROFILE, settings.clone()).collect();
        assert_eq!(g.errors.len(), 1);
        assert!(g.rows.is_empty());
        assert_eq!(g.errors[0].session_id, "gemini:s1");
        assert_eq!(g.errors[0].error_type.as_deref(), Some("rate_limit"), "error.type 优先于 error");
        assert_eq!(g.errors[0].input_tokens, None);
        assert_eq!(g.errors[0].duration_ms, Some(88));

        let q = OtelOutfileSink::new(QWEN_PROFILE, settings).collect();
        assert_eq!(q.errors.len(), 1);
        assert_eq!(q.errors[0].session_id, "qwen-code:s2");
        assert_eq!(q.errors[0].error_type.as_deref(), Some("server_error"));
        assert_eq!(q.errors[0].source_id.as_deref(), Some("otq:e:resp-e1"), "response_id 幂等");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 发现链：enabled 缺失/为 false 不启用；outfile 缺失不启用；
    /// JSONC 注释容忍；`~/` 展开
    #[test]
    fn test_discovery_chain() {
        let dir = tmp_dir("disc");
        let settings = dir.join("settings.json");

        // 未启用：None
        std::fs::write(&settings, r#"{"telemetry": {"outfile": "C:/x.log"}}"#).unwrap();
        assert_eq!(discover_from(&settings, &GEMINI_PROFILE), None);
        // enabled=false：None
        std::fs::write(&settings, r#"{"telemetry": {"enabled": false, "outfile": "C:/x.log"}}"#)
            .unwrap();
        assert_eq!(discover_from(&settings, &GEMINI_PROFILE), None);
        // 启用但无 outfile：None
        std::fs::write(&settings, r#"{"telemetry": {"enabled": true}}"#).unwrap();
        assert_eq!(discover_from(&settings, &GEMINI_PROFILE), None);
        // telemetry 键整个缺失：None
        std::fs::write(&settings, r#"{"theme": "auto"}"#).unwrap();
        assert_eq!(discover_from(&settings, &GEMINI_PROFILE), None);

        // 正常启用＋JSONC 注释容忍＋`~/` 展开
        let settings_text = format!(
            "{{\n  // 用户注释：遥测落盘\n  \"telemetry\": {{\n    \"enabled\": true,\n    \"outfile\": \"{}\"\n  }}\n}}",
            "~/my-telemetry.log"
        );
        std::fs::write(&settings, &settings_text).unwrap();
        let got = discover_from(&settings, &GEMINI_PROFILE).expect("JSONC 注释应被容忍");
        let home = std::env::var_os("USERPROFILE").unwrap();
        assert_eq!(got, PathBuf::from(home).join("my-telemetry.log"));

        // 无效 JSON（注释也无法救回）：None 静默
        std::fs::write(&settings, "not json at all {").unwrap();
        assert_eq!(discover_from(&settings, &GEMINI_PROFILE), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 快轮信号：未配置 → None（settings 无 outfile）；配置但文件未创建 → None；
    /// 文件创建后 → Some（从无到有即变化）
    #[test]
    fn test_hot_signal_states() {
        let dir = tmp_dir("hot");
        let outfile = dir.join("telemetry.log");
        let settings = dir.join("settings.json");
        let settings_text = format!(
            r#"{{"telemetry": {{"enabled": true, "outfile": {}}}}}"#,
            serde_json::to_string(outfile.to_str().unwrap()).unwrap()
        );
        std::fs::write(&settings, settings_text).unwrap();
        let sink = OtelOutfileSink::new(QWEN_PROFILE, settings);
        let sig = sink.hot_signal();
        assert_eq!(sig.sample(), None, "outfile 未创建应无信号");
        std::fs::write(&outfile, "{}\n").unwrap();
        assert!(matches!(sig.sample(), Some(_)), "创建后应有信号");

        // 未启用场景：恒 None（settings mtime 缓存生效，改 mtime 后才重读）
        let settings2 = dir.join("settings2.json");
        std::fs::write(&settings2, r#"{"telemetry": {"enabled": false}}"#).unwrap();
        let sink2 = OtelOutfileSink::new(QWEN_PROFILE, settings2);
        assert_eq!(sink2.hot_signal().sample(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }


    /// 装机后补跑（手动：cargo test -- --ignored test_real_otel_gemini）：
    /// 真实 settings.json 发现＋真实 outfile 全量对账（清单 01-RESEARCH §13.3）。
    /// 断言宽松：未装机/未启用时直接跳过（不算失败）
    #[test]
    #[ignore]
    fn test_real_otel_gemini() {
        let home = match std::env::var_os("USERPROFILE") {
            Some(h) => PathBuf::from(h).join(".gemini"),
            None => return,
        };
        let settings = home.join("settings.json");
        if !settings.exists() {
            eprintln!("本机未装 Gemini CLI（无 settings.json），跳过");
            return;
        }
        let sink = OtelOutfileSink::new(GEMINI_PROFILE, settings);
        let Some(outfile) = discover_from(&home.join("settings.json"), &GEMINI_PROFILE) else {
            eprintln!("未启用 telemetry.outfile（增强档未配置，属预期场景），跳过");
            return;
        };
        eprintln!("outfile：{}", outfile.display());
        let batch = sink.collect();
        eprintln!(
            "gemini otel：token 行 {}（暂不入库），error 行 {}",
            batch.rows.len(),
            batch.errors.len()
        );
        // 幂等键不重复
        let mut ids: Vec<_> = batch.rows.iter().filter_map(|r| r.source_id.clone()).collect();
        ids.sort();
        let n = ids.len();
        ids.dedup();
        assert_eq!(n, ids.len(), "source_id 不得重复");
        // 首读后再采（mtime 未变）应零产出
        assert!(sink.collect().rows.is_empty(), "mtime 未变不应有产出");
    }

    /// 装机后补跑：同上（Qwen 侧）
    #[test]
    #[ignore]
    fn test_real_otel_qwen() {
        let home = match std::env::var_os("USERPROFILE") {
            Some(h) => PathBuf::from(h).join(".qwen"),
            None => return,
        };
        let settings = home.join("settings.json");
        if !settings.exists() {
            eprintln!("本机未装 Qwen Code（无 settings.json），跳过");
            return;
        }
        let Some(_outfile) = discover_from(&settings, &QWEN_PROFILE) else {
            eprintln!("未启用 telemetry.outfile（增强档未配置，属预期场景），跳过");
            return;
        };
        let sink = OtelOutfileSink::new(QWEN_PROFILE, settings);
        let batch = sink.collect();
        eprintln!(
            "qwen otel：token 行 {}（暂不入库），error 行 {}",
            batch.rows.len(),
            batch.errors.len()
        );
        let mut ids: Vec<_> = batch.rows.iter().filter_map(|r| r.source_id.clone()).collect();
        ids.sort();
        let n = ids.len();
        ids.dedup();
        assert_eq!(n, ids.len(), "source_id 不得重复");
    }
}
