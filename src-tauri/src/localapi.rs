//! 本地只读 API（2026-09-29 审查新增 #25，对标 QuotaBar 的 127.0.0.1 本地接口）：
//! 供脚本/小组件/第三方工具读取观测数据——纯只读、仅绑定 127.0.0.1 回环、
//! 默认关闭（红线⑤"打扰一律 opt-in"的暴露面版本：开端口也是一种打扰）。
//! 端点（JSON，UTF-8）：
//!   GET /v1/status  → 岛状态摘要（聚合态/活跃会话数/今日消耗/degraded）
//!   GET /v1/limits  → 全部实例最新额度/余额快照（与额度页同源库读）
//!   GET /v1/spend   → 今日与本月估算成本（与报表预算条同源）
//! 其余路径一律 404；仅 GET；设置键 local_api_enabled（默认关）＋
//! local_api_port（默认 6737）；开关变更重启生效（设置页文案注明）。

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
            log::info!("[本地 API] 已启动：http://{addr}/v1/status｜/v1/limits｜/v1/spend（仅回环，只读）");
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

/// 单请求处理（Host 校验＋只读路由）
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
    // 只读纪律：仅接受 GET，其余 405
    if request.method() != &tiny_http::Method::Get {
        respond_error(request, 405, r#"{"error":"仅支持 GET"}"#);
        return;
    }
    let body = match request.url() {
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
