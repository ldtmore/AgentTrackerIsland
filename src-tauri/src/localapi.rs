//! 本地只读 API（2026-09-29 审查新增 #25，对标 QuotaBar 的 127.0.0.1 本地接口）：
//! 供脚本/小组件/第三方工具读取观测数据——纯只读、仅绑定 127.0.0.1 回环、
//! 默认关闭（红线⑤"打扰一律 opt-in"的暴露面版本：开端口也是一种打扰）。
//! 端点（JSON，UTF-8）：
//!   GET /v1/status  → 岛状态摘要（聚合态/活跃会话数/今日消耗/degraded）
//!   GET /v1/limits  → 全部实例最新额度/余额快照（与额度页同源库读）
//!   GET /v1/spend   → 今日与本月估算成本（与报表预算条同源）
//! 其余路径一律 404；仅 GET；设置键 local_api_enabled（默认关）＋
//! local_api_port（默认 6737）；开关变更重启生效（设置页文案注明）。
//! Token 鉴权（2026-10-16 安全收口）：首次启用自动生成随机 token
//!（local_api_token 键，uuid v4 CSPRNG），请求须携带 Authorization:
//! Bearer <token> 或 ?token=<token>，否则 401——防本机其他进程静默读取
//! 余额/用量（Host 白名单挡的是浏览器跨源，挡不了本机进程）。

use std::sync::Arc;

use crate::store::Store;

/// 渲染 JSON 响应（统一 header＋状态码）
fn respond(
    response: tiny_http::Response<std::io::Cursor<Vec<u8>>>,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    response.with_header(
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..])
            .expect("静态 header 构造不会失败"),
    )
}

/// 确保 token 存在（空/缺失则生成并落库）：随 API 启动调用一次；设置页查询
/// 命令（local_api_token）在启用态惰性调用同函数——用户开开关后无需重启
/// 即可拿到 token。token 走 Rust 内部写入（不经前端 set_setting 白名单）；
/// 查询经专用命令按需下发到本机设置窗口
pub(crate) fn ensure_token(store: &Store) -> String {
    if let Some(t) = store.get_setting("local_api_token").filter(|t| !t.is_empty()) {
        return t;
    }
    let token = uuid::Uuid::new_v4().simple().to_string();
    let _ = store.set_setting("local_api_token", &token);
    log::info!("[本地 API] 已生成访问 token（设置页可查看）");
    token
}

/// 启动本地 API 线程（设置开启时由 lib.rs setup 调用；绑定失败留痕不阻塞应用）
pub fn spawn(store: Arc<Store>, port: u16) {
    std::thread::Builder::new()
        .name("local-api".into())
        .spawn(move || {
            let addr = format!("127.0.0.1:{port}");
            let Ok(server) = tiny_http::Server::http(&addr) else {
                log::warn!("[本地 API] 端口 {port} 绑定失败（被占用？），本地 API 未启动");
                return;
            };
            // 首启确保 token 存在（幂等：已有则沿用——用户脚本里配的 token 不因重启变）
            ensure_token(&store);
            log::info!("[本地 API] 已启动：http://{addr}/v1/status｜/v1/limits｜/v1/spend（仅回环，只读，需 token）");
            for request in server.incoming_requests() {
                // panic 隔离（2026-10-03 审查补充，对照聚合 tick／quota worker 纪律）：
                // 单请求处理 panic 不杀服务线程——否则本地 API 静默失联且无留痕
                if let Err(_) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handle(&store, request, port)
                })) {
                    log::error!("[本地 API] 请求处理 panic，已隔离继续服务");
                }
            }
        })
        .expect("本地 API 线程启动失败");
}

/// 单请求处理（Host 校验＋token 鉴权＋只读路由）。
/// token 每请求现读（第七轮 4.3）：原先闭包固化启动时的值，设置页重置后
/// 旧 token 仍可用到重启——回环 API 低频，SQLite 点查微秒级，现读换「重置
/// 立即生效」的安全闭环；键缺失/为空（未生成）时按 401 语义拒绝
fn handle(store: &Store, request: tiny_http::Request, port: u16) {
    // Host 白名单（2026-10-03 审查修复，防 DNS rebinding 跨源读取）：浏览器同源
    // 判定只看主机名——攻击者域名 rebinding 到 127.0.0.1 后，其页面与本服务
    // 同源（同主机同端口），无 CORS 头也能完整读取响应。只接受字面回环主机名
    let host_ok = request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_bytes().eq_ignore_ascii_case(b"host"))
        .and_then(|h| Some(h.value.as_str()))
        .map(|h| h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"))
        .unwrap_or(false);
    if !host_ok {
        respond_error(request, 403, r#"{"error":"Host 不在白名单"}"#);
        return;
    }
    // Token 鉴权（2026-10-16）：Bearer 头或 query 参数二选一（curl/脚本两形态
    // 都顺手）；恒时比较免理论时序侧信道（固定长度 uuid simple，逐字节异或）。
    // 现读库值（第七轮 4.3）：空/缺失（未生成）恒 401
    let url = request.url().to_string();
    let token = store.get_setting("local_api_token").unwrap_or_default();
    let bearer = request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_bytes().eq_ignore_ascii_case(b"authorization"))
        .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
        .unwrap_or("");
    let query_token = url
        .split_once('?')
        .and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("token=")))
        .unwrap_or("");
    let token_ok = !token.is_empty()
        && (bearer.len() == token.len() || query_token.len() == token.len())
        && (const_eq(bearer, &token) || const_eq(query_token, &token));
    if !token_ok {
        log::warn!("[本地 API] 请求被拒（token 缺失或不符）");
        respond_error(request, 401, r#"{"error":"需要 token（Authorization: Bearer 或 ?token=，设置页可查看）"}"#);
        return;
    }
    // 只读纪律：仅接受 GET，其余 405
    if request.method() != &tiny_http::Method::Get {
        respond_error(request, 405, r#"{"error":"仅支持 GET"}"#);
        return;
    }
    // 路径匹配剥离 query 段（token 经 query 传入时 url 含 ?token=...）
    let path = url.split('?').next().unwrap_or("");
    let body = match path {
        "/v1/status" => status_body(store),
        "/v1/limits" => limits_body(store),
        "/v1/spend" => spend_body(store),
        _ => {
            respond_error(request, 404, r#"{"error":"未知路径"}"#);
            return;
        }
    };
    let _ = request.respond(respond(tiny_http::Response::from_data(body)));
}

/// 恒时字符串比较（长度不等直接 false，避免短路时序差）
fn const_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 错误响应便捷封装（消耗 request——respond 按值收）
fn respond_error(request: tiny_http::Request, code: u32, msg: &str) {
    let _ = request.respond(
        respond(tiny_http::Response::from_string(msg.to_string()).with_status_code(code)),
    );
}

/// 手工拼 JSON（观测载荷字段全为标量/简单数组；字符串字段全量转义——
/// 内容来自内部注册表与用户自起的别名，别名可含任意字符，控制字符（<0x20）
/// 不转义会产生非法 JSON（2026-10-10 审查修复：原仅转义五类基础字符）
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // 其余控制字符按 \u00XX 转义（JSON 规范要求）
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn status_body(store: &Store) -> Vec<u8> {
    // 启用实例计数（四轮审查）：原先组装 island_accounts 完整视图（逐实例
    // latest_balance 点查等）却只取 len——语义同为「启用实例数」，改直接计数
    let accounts = store
        .list_provider_accounts()
        .iter()
        .filter(|a| a.enabled)
        .count();
    let sessions = store.recent_session_count();
    let (today_tokens, today_calls) = store.today_usage_now();
    let body = format!(
        "{{\"generated_at\":{},\"accounts\":{},\"active_sessions\":{},\"today_tokens\":{},\"today_calls\":{}}}",
        now_ms(),
        accounts,
        sessions,
        today_tokens,
        today_calls,
    );
    body.into_bytes()
}

fn limits_body(store: &Store) -> Vec<u8> {
    let quotas = store.latest_quotas();
    let accounts = crate::state::service::island_accounts(store, &quotas);
    let mut items: Vec<String> = vec![];
    for a in accounts {
        let mut qjson: Vec<String> = a
            .quotas
            .iter()
            .map(|q| {
                format!(
                    "{{\"window\":\"{}\",\"used_percent\":{},\"reset_at\":{},\"fetched_at\":{}}}",
                    esc(&q.window_kind),
                    q.used_percent.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
                    q.reset_at.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
                    q.fetched_at
                )
            })
            .collect();
        let bjson = a.balance.as_ref().map(|b| {
            format!(
                "{{\"currency\":\"{}\",\"total\":{:.4},\"fetched_at\":{}}}",
                esc(&b.currency),
                b.total,
                b.fetched_at
            )
        });
        items.push(format!(
            "{{\"id\":\"{}\",\"kind\":\"{}\",\"name\":\"{}\",\"alias\":\"{}\",\"in_island\":{},\"quotas\":[{}],\"balance\":{}}}",
            esc(&a.id),
            esc(&a.kind_id),
            esc(&a.kind_name),
            esc(&a.alias),
            a.in_island,
            qjson.join(","),
            bjson.unwrap_or_else(|| "null".into()),
        ));
        qjson.clear();
    }
    format!("{{\"accounts\":[{}]}}", items.join(",")).into_bytes()
}

fn spend_body(store: &Store) -> Vec<u8> {
    let (today, month) = store.budget_usage().unwrap_or((None, None));
    let f = |v: Option<f64>| v.map(|x| format!("{x:.4}")).unwrap_or_else(|| "null".into());
    format!(
        "{{\"today_cost_usd\":{},\"month_cost_usd\":{}}}",
        f(today),
        f(month),
    )
    .into_bytes()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// 临时库路径（对齐 store::tests::tmp_db 形态）
    fn tmp_db(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("at-test-{tag}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// 原始 TCP 发一次带 token 的 status 请求，返回响应状态码（handle 全链路：
    /// Host 校验 → 现读鉴权 → 路由）
    fn status_code(port: u16, token: &str) -> u16 {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            s,
            "GET /v1/status?token={token} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut buf = String::new();
        s.read_to_string(&mut buf).unwrap();
        buf.split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0)
    }

    /// token 重置立即生效（第七轮 4.3 回归锁）：API 服务线程每请求现读库——
    /// 重置（置空＋重新生成）后旧 token 立即 401、新 token 200。修复前 handle
    /// 固化启动时的 token，重置后旧值仍可用到重启（断言 401 失败）
    #[test]
    fn test_token_reset_invalidates_old() {
        let path = tmp_db("localapi-reset");
        let store = Arc::new(Store::open(&path).unwrap());
        // 起随机端口服务（与 spawn 同款循环形态；服务线程随测试进程退出回收）
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let store2 = store.clone();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handle(&store2, request, port)
                }));
            }
        });
        // 初始 token：请求携带 → 200
        let old = ensure_token(&store);
        assert_eq!(status_code(port, &old), 200, "初始 token 应放行");
        assert_eq!(status_code(port, "wrong-token"), 401, "错误 token 应拒绝");
        // 模拟重置命令（local_api_token_reset 的库语义：置空 → ensure_token 生成新值）
        store.set_setting("local_api_token", "");
        let new = ensure_token(&store);
        assert_ne!(old, new, "重置后应生成新 token");
        // 核心：旧 token 立即失效（现读；修复前固化值会放行）
        assert_eq!(status_code(port, &old), 401, "重置后旧 token 应立即 401");
        assert_eq!(status_code(port, &new), 200, "重置后新 token 应放行");
        let _ = std::fs::remove_file(&path);
    }
}
