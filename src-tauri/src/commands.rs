//! 窗口跳转（T10）：点击会话卡片激活对应的终端/IDE 窗口（窗口级定位）。
//! 匹配策略（优先级从高到低）：窗口标题含完整项目路径 > 含项目目录名 >
//! Agent 关键词（zcode 桌面窗口 / claude 终端）；全部未命中返回 false。

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    SetForegroundWindow, ShowWindow, SW_RESTORE,
};

// 枚举结果的 thread_local 收集器（模块级单实例，回调与读取共用）
// 元组：（句柄， 标题， 所属进程 PID）
thread_local! {
    static FOUND: std::cell::RefCell<Vec<(isize, String, u32)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// 枚举当前全部可见顶层窗口，返回 （句柄， 标题， PID）
pub fn collect_visible_windows() -> Vec<(isize, String, u32)> {
    FOUND.with(|f| f.borrow_mut().clear());
    unsafe {
        // SAFETY：回调只写入 thread_local 向量；EnumWindows 同步完成
        let _ = EnumWindows(Some(enum_proc), LPARAM(0));
    }
    FOUND.with(|f| f.borrow().clone())
}

/// SAFETY：仅读写 thread_local 向量，不做其他不安全操作
unsafe extern "system" fn enum_proc(hwnd: HWND, _: LPARAM) -> BOOL {
    if !hwnd.0.is_null() && IsWindowVisible(hwnd).as_bool() {
        let mut buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len > 0 {
            let title = String::from_utf16_lossy(&buf[..len as usize]);
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            FOUND.with(|f| f.borrow_mut().push((hwnd.0 as isize, title, pid)));
        }
    }
    BOOL(1) // 继续枚举
}

/// 按会话信息寻找最佳匹配窗口句柄
/// 匹配策略：标题含完整项目路径 > 含项目目录名 > 进程链匹配（跑 claude 的终端，
/// 解决 Windows Terminal 标题不含路径的问题）> Agent 关键词兜底
pub fn find_session_window(agent: &str, project_dir: Option<&str>) -> Option<isize> {
    let windows = collect_visible_windows();

    // ① 标题含完整项目路径（终端标题常显示当前目录）
    if let Some(dir) = project_dir {
        let target = dir.to_ascii_lowercase().replace('/', "\\");
        if !target.is_empty() {
            if let Some((h, _, _)) = windows
                .iter()
                .find(|(_, t, _)| t.to_ascii_lowercase().contains(&target))
            {
                log::debug!("[跳转] 策略①标题含完整路径命中：hwnd={h}");
                return Some(*h);
            }
        }
        // ② 标题含项目目录末段（如 AgentTrackerIsland）
        if let Some(name) = dir.rsplit(['\\', '/']).next().filter(|s| !s.is_empty()) {
            let name = name.to_ascii_lowercase();
            if name.len() >= 3 {
                // 只拦 1~2 字符目录名（误匹配率高）；3 字符常见短名（src/doc/app
                // 等）保留命中——跳过会漏掉真实项目目录，误匹配为已知取舍
                //（2026-10-10 审查对齐：原注释示例「如 src」与 >=3 的实际行为不符）
                if let Some((h, _, _)) = windows
                    .iter()
                    .find(|(_, t, _)| t.to_ascii_lowercase().contains(&name))
                {
                    log::debug!("[跳转] 策略②标题含目录名「{name}」命中：hwnd={h}");
                    return Some(*h);
                }
            }
        }
    }

    // ③ 进程链匹配：找到命令行在跑 claude 的终端进程，沿父链爬到宿主窗口
    // （Windows Terminal 标签无路径信息，但 pwsh/node 是 WT 子进程，按 PID 反查窗口）
    if agent == "claude-code" {
        if let Some(h) = find_terminal_running_claude(&windows) {
            log::debug!("[跳转] 策略③进程链命中：hwnd={h}");
            return Some(h);
        }
    }

    // ④ Agent 关键词兜底：zcode 桌面窗口 / claude 终端
    let keyword = if agent == "zcode" { "zcode" } else { "claude" };
    let hit = windows
        .iter()
        .find(|(_, t, _)| t.to_ascii_lowercase().contains(keyword))
        .map(|(h, _, _)| *h);
    if let Some(h) = hit {
        log::debug!("[跳转] 策略④关键词「{keyword}」命中：hwnd={h}");
    } else {
        // 跳转未命中诊断（T10）：走文件日志（2026-09-17 埋点审查：
        // 原 eprintln 在 release 无控制台等于丢失，用户报"跳不过去"时零线索）
        log::debug!(
            "[跳转] 全部策略未命中：agent={agent} project_dir={project_dir:?}，可见窗口 {} 个：",
            windows.len()
        );
        for (h, t, pid) in windows.iter().take(40) {
            let title: String = t.chars().take(60).collect();
            log::debug!("[跳转]   hwnd={h} pid={pid} title={title}");
        }
    }
    hit
}

/// 在跑 claude 的终端进程 → 其（祖先）顶层窗口
pub(crate) fn find_terminal_running_claude(windows: &[(isize, String, u32)]) -> Option<isize> {
    use sysinfo::{ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);

    // 候选：shell/node 进程且命令行含 claude（排除自身与菜单脚本误报）
    let mut wanted_pids: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for (pid, proc) in sys.processes() {
        let name = proc.name().to_string_lossy().to_ascii_lowercase();
        let is_shell = name.contains("pwsh")
            || name.contains("powershell")
            || name.contains("node")
            || name.contains("cmd")
            || name.contains("claude");
        if !is_shell {
            continue;
        }
        let cmd = proc
            .cmd()
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<String>()
            .to_ascii_lowercase();
        if cmd.contains("claude")
            && !cmd.contains("agenttrackerisland")
            && !cmd.contains("claude-menu")
        {
            // 沿父进程链全部标记（直到 WT 宿主/无父）
            let mut cur = Some(*pid);
            while let Some(p) = cur {
                #[cfg(windows)]
                let as_u32 = p.as_u32();
                #[cfg(not(windows))]
                let as_u32 = u32::try_from(p.0).unwrap_or(0);
                if !wanted_pids.insert(as_u32) {
                    break; // 环，防御
                }
                cur = sys.process(p).and_then(|pr| pr.parent());
            }
        }
    }
    if wanted_pids.is_empty() {
        log::debug!("[跳转] 进程链未找到在跑 claude 的终端进程");
        return None;
    }
    log::debug!("[跳转] 进程链候选 PID：{:?}", wanted_pids);
    // 窗口按 PID 命中（优先标题最长的，避免 "Default" 之类空壳）
    let mut hits: Vec<&(isize, String, u32)> = windows
        .iter()
        .filter(|(_, _, pid)| wanted_pids.contains(pid))
        .collect();
    hits.sort_by_key(|(_, t, _)| std::cmp::Reverse(t.len()));
    hits.first().map(|(h, _, _)| *h)
}

/// 激活窗口：最小化时先还原＋置前台。
/// SW_RESTORE 对**最大化**窗口也会还原为普通尺寸（MSDN 语义），用户最大化使用
/// 终端时点卡片跳转会被「压小」——故仅 IsIconic（最小化）时才 restore
pub fn activate_window(handle: isize) -> bool {
    let hwnd = HWND(handle as *mut core::ffi::c_void);
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd).as_bool()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实桌面环境验证（手动：cargo test -- --ignored test_real_windows）
    #[test]
    #[ignore]
    fn test_real_windows() {
        let wins = collect_visible_windows();
        assert!(!wins.is_empty(), "桌面应有可见窗口");
        println!("可见窗口 {} 个，样例：", wins.len());
        for (h, t, pid) in wins.iter().filter(|(_, t, _)| !t.trim().is_empty()).take(5) {
            println!("  [{h}] pid={pid} {}", t.chars().take(50).collect::<String>());
        }
        // zcode 桌面在跑：关键词应命中
        assert!(find_session_window("zcode", None).is_some(), "应能找到 zcode 窗口");
        // claude-code：进程链匹配（本机有 claude 在 WT 中运行）
        if let Some(h) = find_session_window("claude-code", None) {
            println!("claude-code 进程链命中窗口：{h}");
        } else {
            println!("（当前无运行中的 claude 终端，进程链未命中属正常）");
        }
    }
}

// ===== 以下自 lib.rs 拆入（2026-10-03 四轮审查，纯移动零逻辑变更）：
// 全部 Tauri 命令与命令层辅助（设置/供应商实例/hooks/自启/报表与会话查询/
// CSV 导出/窗口唤起）。窗口跳转的 find_session_window 原本就在本文件。
use std::sync::Arc;
use tauri::Manager;
use crate::store::Store;
use crate::now_ms;


/// 点击会话卡片 → 激活对应终端/IDE 窗口（T10，窗口级定位）。
/// async＋spawn_blocking（2026-10-03 审查修复）：claude-code 策略③全量枚举
/// 进程＋逐进程读命令行在 Windows 上 100~500ms，同步命令跑主线程会冻结全部
/// 窗口（Windows Terminal 标题不含路径时必然走到该分支）
#[tauri::command]
pub(crate) async fn focus_session(session_id: String, store: tauri::State<'_, Arc<Store>>) -> Result<bool, String> {
    // async 命令借用 State<'_> 须返回 Result（Tauri 宏约束）；前端两处调用均
    // .catch 兜底，错误路径无行为差异
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Some((agent, project_dir)) = store.get_session_meta(&session_id) else {
            // 点击跳转无反应的根因之一：会话不在自库（扫描截断/未采集到）
            log::debug!("[跳转] 会话元数据缺失（库里查不到）：{session_id}");
            return false;
        };
        let ok = find_session_window(&agent, project_dir.as_deref())
            .map(activate_window)
            .unwrap_or(false);
        // 窗口找到了但激活失败（SetForegroundWindow 可能被系统拒绝）单独留痕
        if !ok {
            log::debug!("[跳转] 窗口已找到但激活失败：agent={agent} session={session_id}");
        }
        ok
    })
    .await
    .map_err(|e| {
        log::warn!("[跳转] 后台任务失败：{e}");
        format!("跳转任务失败：{e}")
    })
}

// ===== 前端日志通道（2026-09-17 埋点审查 P2） =====

/// 前端日志级别白名单（防任意字符串透传）
pub(crate) const FRONTEND_LEVELS: &[&str] = &["error", "warn", "info", "debug"];

/// 前端（webview）日志落文件：全局 onerror / unhandledrejection / ErrorBoundary
/// 与关键交互手动埋点统一走这里。窗口名取自 Tauri 窗口 label（前端不用传），
/// target 固定 frontend，与 Rust 侧日志同一文件同一格式。
#[tauri::command]
pub(crate) fn log_frontend(win: tauri::WebviewWindow, level: String, message: String) {
    if !FRONTEND_LEVELS.contains(&level.as_str()) {
        return; // 白名单外的级别直接丢弃（防御异常输入）
    }
    // 按字符截断到 1024：防异常对象序列化出巨串刷爆日志
    //（不能按字节切——撕裂 UTF-8 会 panic，日志通道内绝不允许 panic）
    let message: String = message.chars().take(1024).collect();
    let lvl = match level.as_str() {
        "error" => log::Level::Error,
        "warn" => log::Level::Warn,
        "info" => log::Level::Info,
        _ => log::Level::Debug,
    };
    log::log!(target: "frontend", lvl, "[{}] {}", win.label(), message);
}

// ===== 设置页 commands（T11） =====

/// 允许前端写入的设置键白名单（审查 2.1.3）：防止任意键写入
/// （如覆盖 hook_events_offset/island_pos 等内部状态键）。
/// glm_base/glm_token 已移除（2026-10-10 审查）：M3-2 迁移后两键无任何消费者
/// （bootstrap 一次性迁移直读 DB，不走本白名单；存量行保留兼容回滚），
/// 白名单残留意味着明文 API Key 可绕过钥匙串直写 SQLite 并经 get_settings 回显。
/// 凭据写入一律走 provider_account_create（真值进钥匙串）
pub(crate) const SETTING_KEYS_ALLOW: &[&str] = &[
    "threshold_warn",
    "threshold_danger",
    "cleanup_days",
    "island_autohide",
    "island_opacity",
    "hover_expand",
    "tray_left_action",
    "agents_enabled",
    "agent_colors",
    // Agent 卡片拖拽排序（2026-10-03）：JSON 数组（完整 id 置换）。不做格式
    // 校验——读取方（前端 parseAgentOrder / 后端 report_options）均防御性
    // 解析，缺失或损坏一律回退 AGENT_DEFS 默认序
    "agents_order",
    // 首次引导「三步上手卡」一次性键（07-UX 1.1）：三步全完成或点「不再显示」
    // 落 1，之后设置页不再渲染引导卡
    "onboarding_done",
    "theme",
    "dev_mode",
    // M3-4：额度查询总开关（调度器每 tick 现读）与余额警戒线（M3-6 托盘/岛告急口径；
    // M3-7 起按币种分设：_usd 后缀管美元账户，其余币种暂无线）
    "quota_fetch_enabled",
    "quota_balance_warn",
    "quota_balance_warn_usd",
    // M3-12：Claude 订阅档位（auto=近 8 天 P90 自动探测；pro/max5/max20 固定限额）
    "claude_plan",
    // #23 日/月消费预算（美元，与报表估算成本同口径；0=不设）：报表页预算条
    // 与超线告警的依据，对标 QuotaBar/LiteLLM 的 spend budget
    "budget_daily_usd",
    "budget_monthly_usd",
    // #25 本地只读 API（默认关——开端口也是打扰，红线⑤ opt-in；仅回环只读）；
    // 开关/端口变更重启生效
    "local_api_enabled",
    "local_api_port",
    // 岛显隐全局快捷键（07-UX 2.3）：存组合串（ctrl+alt+i），空值＝未启用，
    // 键缺失＝默认启用；set_setting 命中即同步注册（失败报错不落库）
    "hotkey_toggle_island",
    // 会话等待输入系统通知（07-UX 2.6，默认关——红线⑤ opt-in 本体）：
    // "1"＝开启；聚合器每 tick 现读
    "notify_waiting",
    // 灵动岛上岛深度（2026-10-08「在岛展示前 N 个」）：拖拽序前 N 个 Agent
    // 上岛（贴边分段只渲染这些）。整数串（1~6），不做格式校验——读取方
    // （前端 parseAgentLimit）防御性解析，缺失/损坏/越界一律归一默认 5
    "island_agent_limit",
];

/// 读取全部设置。
/// glm_token 原样随设置下发（2026-09-17 所有者要求 API Key 回显输入框，
/// 推翻原审查 2.1.2"敏感值不下发前端"的决策；仅下发到本机自身窗口）
#[tauri::command]
pub(crate) fn get_settings(store: tauri::State<'_, Arc<Store>>) -> std::collections::HashMap<String, String> {
    store.all_settings()
}

/// 写单条设置（白名单外的键拒绝并报错，前端会显示"保存失败"）。
/// app 参数（07-UX 2.3 新增，Tauri 自动注入）：全局快捷键键需即时同步注册状态
#[tauri::command]
pub(crate) fn set_setting(
    key: String,
    value: String,
    app: tauri::AppHandle,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<(), String> {
    if !SETTING_KEYS_ALLOW.contains(&key.as_str()) {
        log::warn!("拒绝写入未登记的设置键：{key}");
        return Err(format!("不允许写入设置键：{key}"));
    }
    // 余额警戒线（人民币/美元两键同规则）与消费预算（#23）：必须是 ≥0 的数字
    // （前端已校验，此处后端兜底）
    if key == "quota_balance_warn"
        || key == "quota_balance_warn_usd"
        || key == "budget_daily_usd"
        || key == "budget_monthly_usd"
    {
        let ok = value.trim().parse::<f64>().map(|v| v.is_finite() && v >= 0.0).unwrap_or(false);
        if !ok {
            return Err("该设置必须是不小于 0 的数字".into());
        }
    }
    // 全局快捷键（07-UX 2.3）：先注册后落库——注册失败（被占用/组合串非法）
    // 返回 Err 让前端 toast 并回滚显示，库保持旧值（「提示且不假开」）；
    // 失败路径尽力回注旧值（旧值一般能注册成功），防「库说开着、实际没注册」
    if key == "hotkey_toggle_island" {
        if let Err(e) = crate::tray::apply_island_hotkey(&app, &value) {
            let old = store
                .get_setting("hotkey_toggle_island")
                .unwrap_or_else(|| crate::tray::HOTKEY_ISLAND_COMBO.into());
            let _ = crate::tray::apply_island_hotkey(&app, &old);
            return Err(e);
        }
    }
    store.set_setting(&key, &value);
    // 设置变更留痕（info：低频关键事件，事后排障不依赖用户提前开开发者模式）。
    // 留警（2026-10-10 移除 glm_token 特例时）：未来若新增涉密设置键（凭据类），
    // 必须沿用「只记长度不记明文」的特例处理——日志文件会被用户分享出去；
    // 现凭据已全走 provider_account_create → 钥匙串，此路径无密钥流经
    log::info!("[设置] {key} = {value}");
    // 开发者模式即时切换日志级别（免重启）
    if key == "dev_mode" {
        crate::logging::set_verbose(value == "1");
    }
    Ok(())
}

// ===== 供应商实例 commands（M3-4，06-PLAN §7：设置页供应商管理） =====
// 厂商注册表是 Rust 静态清单（单一真值源），实例是纯数据（SQLite）；凭据真值
// 只进钥匙串/环境变量，SQLite 不落盘、日志与返回值均不回显

/// 厂商注册表视图（前端 chip 墙数据源：品牌色/默认端点/凭据 ⓘ 全部带出）
#[derive(serde::Serialize)]
pub(crate) struct KindView {
    id: String,
    name: String,
    color: String,
    default_base: String,
    /// "windows" | "balance" | "local_estimate"（前端按口径分型）
    quota_kind: String,
    currency: String,
    cred_hint: String,
    /// 本地推算厂商（M3-12）：无凭据/无端点，表单隐藏凭据与接口地址行
    local_only: bool,
}

/// 额度刷新通知通道（2026-09-29 审查修复 #16）：额度页「查询」
/// （provider_account_refresh）成功落库后，经此通知额度工作线程复位该实例
/// 的退避水位——手动刷新与调度器不再按旧退避原地重复外呼
pub(crate) struct QuotaRefreshNotify(pub(crate) std::sync::mpsc::SyncSender<String>);

/// 展示层常量下发（2026-09-29 审查修复 #20）：「已结束」判定阈值此前是
/// Rust/TS 两处手工同步的字面量，漂移即口径分裂——后端单真值源，前端
/// （岛面板与会话窗口）挂载时拉取一次填充 sessionDisplay 模块内变量
#[tauri::command]
pub(crate) fn display_constants() -> std::collections::HashMap<&'static str, i64> {
    std::collections::HashMap::from([("ended_after_ms", crate::store::ENDED_AFTER_MS)])
}

/// pace 燃烧速度预测（2026-09-29 审查新增 #21）：某实例某窗口近 2h 采样点
/// 求斜率，外推"重置时刻的预测值／到限剩余分钟"——额度页窗口条下的小字预警。
/// async＋spawn_blocking（2026-10-03 审查修复）：走库查询的重命令统一离主线程
#[tauri::command]
pub(crate) async fn quota_pace(
    account_id: String,
    window_kind: String,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<Option<crate::store::PaceResult>, String> {
    run_report(store, move |s| {
        let now = now_ms();
        let pts = s.quota_pace_points(&account_id, &window_kind, now - 2 * 3_600_000);
        if pts.is_empty() {
            return Ok(None); // 该窗口暂无采样（从未刷新/停用实例），前端不显示
        }
        // reset_at 取最新快照的值（与额度页展示同源）
        let reset_at = s
            .latest_quotas()
            .into_iter()
            .find(|q| q.account_id.as_deref() == Some(account_id.as_str()) && q.window_kind == window_kind)
            .and_then(|q| q.reset_at);
        Ok(crate::store::compute_pace(&pts, reset_at))
    })
    .await
}

/// 消费预算用量（#23）：今日／本月估算成本（USD），报表页预算条数据源。
/// async＋spawn_blocking（2026-10-03 审查修复）：报表页挂载即调，重查询不冻结主线程
#[tauri::command]
pub(crate) async fn budget_usage(
    store: tauri::State<'_, Arc<Store>>,
) -> Result<Option<(Option<f64>, Option<f64>)>, String> {
    run_report(store, |s| Ok(s.budget_usage())).await
}

/// 年度热力图数据（#24）：近 365 天逐日 (日期, token, 次数)；
/// 恒全量口径，不随报表页筛选联动（2026-09-29 评审后暂缓，待口径重设计）。
/// async＋spawn_blocking（2026-10-03 审查修复）：365 天全表 GROUP BY 属重查询
#[tauri::command]
pub(crate) async fn year_heatmap(
    store: tauri::State<'_, Arc<Store>>,
) -> Result<Vec<(String, i64, i64)>, String> {
    run_report(store, |s| Ok(s.year_heatmap())).await
}

/// 内置厂商注册表（设置页 chip 墙按此渲染，前端零硬编码）
#[tauri::command]
pub(crate) fn provider_kinds() -> Vec<KindView> {
    crate::provider::kinds()
        .iter()
        .map(|k| KindView {
            id: k.id.into(),
            name: k.name.into(),
            color: k.color.into(),
            default_base: k.default_base.into(),
            quota_kind: match k.quota_kind {
                crate::provider::QuotaKind::Windows => "windows",
                crate::provider::QuotaKind::Balance => "balance",
                crate::provider::QuotaKind::LocalEstimate => "local_estimate",
            }
            .into(),
            currency: k.currency.into(),
            cred_hint: k.cred_hint.into(),
            local_only: k.local_only,
        })
        .collect()
}

/// 选择器条目视图（2026-10-10 双站变体机制）：KindView 的变体展开形态——
/// 双站厂商展开为国内/国际两条（成对戴徽标），单站厂商一条无徽标。
/// 添加表单与厂商选择器的唯一数据源（真值源在注册表，前端哑渲染）
#[derive(serde::Serialize)]
pub(crate) struct KindEntryView {
    /// 选中后写入实例的 kind_id
    kind_id: String,
    /// 变体键（"cn"/"intl"；单站厂商空串）
    variant_key: String,
    /// 条目主名（变体 display_name 或 kind.name；官方定名，不拼站别）
    name: String,
    /// kind 级显示名（编辑态 base 反推不到变体时的兜底文案；=kind.name）
    kind_name: String,
    /// 搜索别名（kind 级，复制到每条目；与条目名/域名共同参与前端过滤，
    /// 永不上屏）
    aliases: Vec<String>,
    /// 站别徽标文案（"国内站"/"国际站"；单站厂商空串＝不渲染徽标）
    badge: String,
    /// 副行俗名（空＝该条目副行只显域名）
    alt_name: String,
    /// 副行域名（default_base 的 host 部分）
    domain: String,
    /// 选中后写入的 base_override（默认变体=空串＝不回填，存储走「空=kind
    /// 默认」语义；国际变体=完整端点，必须显式落库否则请求回落国内站）
    base: String,
    /// 该条目所属 kind 的默认端点（保存归一化：base==它时存 null）
    kind_default_base: String,
    /// 新建实例默认别名
    default_alias: String,
    /// 凭据指引（变体级覆盖 kind 级，如 Moonshot 国际站 401 警示）
    cred_hint: String,
    color: String,
    quota_kind: String,
    currency: String,
    local_only: bool,
}

/// 选择器条目清单：变体展开后的扁平列表（顺序=注册表序，变体按声明序
/// 紧邻展开——GLM/Z.ai 相邻，不引入排序键）
#[tauri::command]
pub(crate) fn provider_kind_entries() -> Vec<KindEntryView> {
    crate::provider::kinds()
        .iter()
        .flat_map(|k| {
            let quota_kind = match k.quota_kind {
                crate::provider::QuotaKind::Windows => "windows",
                crate::provider::QuotaKind::Balance => "balance",
                crate::provider::QuotaKind::LocalEstimate => "local_estimate",
            };
            // 变体展开：每变体一条（徽标/显示名/端点/别名/指引全走变体级）；
            // 单站厂商一条（badge 空、base 空＝存储语义「空=kind 默认」）
            let entries: Vec<(Option<&crate::provider::ProviderVariant>, &str)> = if k
                .variants
                .is_empty()
            {
                vec![(None, "")]
            } else {
                k.variants.iter().map(|v| (Some(v), v.key)).collect()
            };
            entries
                .into_iter()
                .map(move |(v, vkey)| {
                    let base = v.map(|v| v.default_base).unwrap_or("");
                    let base = if k.variants.is_empty() || v.map(|v| v.key) == Some(k.variants[0].key) {
                        // 默认变体不回填（空=kind 默认，保留「厂商换域名自动跟随」语义）
                        ""
                    } else {
                        base
                    };
                    KindEntryView {
                        kind_id: k.id.into(),
                        variant_key: vkey.into(),
                        name: v.map(|v| v.display_name).unwrap_or(k.name).into(),
                        kind_name: k.name.into(),
                        aliases: k.aliases.iter().map(|s| s.to_string()).collect(),
                        badge: v.map(|v| v.badge).unwrap_or("").into(),
                        alt_name: v
                            .map(|v| v.alt_name)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(k.alt_name)
                            .into(),
                        domain: crate::provider::host_of(if base.is_empty() {
                            k.default_base
                        } else {
                            base
                        })
                        .into(),
                        base: base.into(),
                        kind_default_base: k.default_base.into(),
                        default_alias: v.map(|v| v.default_alias).unwrap_or(k.name).into(),
                        cred_hint: v.map(|v| v.cred_hint).unwrap_or(k.cred_hint).into(),
                        color: k.color.into(),
                        quota_kind: quota_kind.into(),
                        currency: v.map(|v| v.currency).unwrap_or(k.currency).into(),
                        local_only: k.local_only,
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// 实例最新余额视图（设置页卡片摘要；岛侧 M3-6 才消费）
#[derive(serde::Serialize)]
pub(crate) struct BalanceView {
    currency: String,
    total: f64,
    granted: Option<f64>,
    fetched_at: i64,
}

/// 实例卡片视图（设置页列表数据源：账号字段＋最新快照＋凭据解析状态）
#[derive(serde::Serialize)]
pub(crate) struct AccountOverview {
    id: String,
    kind_id: String,
    /// 运行时厂商显示名（2026-10-10 双站变体投影：按 base 反推变体取
    /// display_name，Z.ai 实例不再显示「GLM」；前端不再自行从 kindsMap 拼名）
    kind_name: String,
    alias: String,
    base_override: Option<String>,
    /// 'plain'（真值在钥匙串不回显）| 'env'（cred_value 为变量名，非敏感可回显）
    cred_kind: String,
    cred_value: Option<String>,
    note: Option<String>,
    enabled: bool,
    in_island: bool,
    origin: String,
    created_at: i64,
    updated_at: i64,
    /// 凭据解析状态：None=可用；Some=分类文案（卡片红字提示，与网络失败区分）
    cred_error: Option<String>,
    /// 脱敏凭据回显（2026-10-10，前 8＋…＋尾 4 三档规则）：编辑表单「认 key」
    /// 用——复用下方逐实例钥匙串读取顺带产出，零额外读取；真值永不进前端。
    /// None＝凭据不可用（cred_error 非空）或 local_only/env 模式
    cred_masked: Option<String>,
    /// 该实例最新窗口快照（Windows 口径）
    quotas: Vec<crate::state::service::QuotaView>,
    /// 该实例最新余额快照（Balance 口径）
    balance: Option<BalanceView>,
}

/// 实例列表（含每实例最新快照与凭据状态）。四轮审查：改 async＋run_report——
/// 逐实例钥匙串读取（Windows 凭据管理器繁忙时可拖慢）＋2N 次查询不该在主线程
/// 串行（同步 command 冻结包括岛在内的全部窗口）；失败显式 Err 由前端 toast/错误态呈现
#[tauri::command(async)]
pub(crate) async fn provider_overview(
    store: tauri::State<'_, Arc<Store>>,
) -> Result<Vec<AccountOverview>, String> {
    run_report(store, |store| {
        let latest = store.latest_quotas();
        let out: Vec<AccountOverview> = store
            .list_provider_accounts()
            .into_iter()
            .map(|a| {
                // 凭据状态与脱敏回显一次读取两产出：resolve_creds 本就为 cred_error
                // 逐实例读钥匙串，Ok 分支的明文在此顺带脱敏（2026-10-10）
                let (cred_error, cred_masked) = match crate::provider::resolve_creds(&a) {
                    Err(s) => (Some(s.describe_short()), None),
                    Ok(creds) if creds.key.is_empty() => (None, None),
                    Ok(creds) => (None, Some(crate::provider::mask_key(&creds.key))),
                };
                let quotas = latest
                    .iter()
                    .filter(|q| q.account_id.as_deref() == Some(a.id.as_str()))
                    .map(crate::state::service::QuotaView::from)
                    .collect();
                let balance = store.latest_balance(&a.id).map(|b| BalanceView {
                    currency: b.currency,
                    total: b.total,
                    granted: b.granted,
                    fetched_at: b.fetched_at,
                });
                AccountOverview {
                    id: a.id,
                    kind_id: a.kind_id.clone(),
                    kind_name: crate::provider::runtime_kind_name(
                        &a.kind_id,
                        a.base_override.as_deref(),
                    ),
                    alias: a.alias,
                    base_override: a.base_override,
                    cred_kind: a.cred_kind,
                    cred_value: a.cred_value,
                    note: a.note,
                    enabled: a.enabled,
                    in_island: a.in_island,
                    origin: a.origin,
                    created_at: a.created_at,
                    updated_at: a.updated_at,
                    cred_error,
                    cred_masked,
                    quotas,
                    balance,
                }
            })
            .collect();
        Ok(out)
    })
    .await
}

/// 凭据与接口地址的限长兜底（2026-10-10 举一反三，与表单 maxLength 同规则）：
/// 超长输入无崩溃风险（String/SQLite），但 base 无索引约束而 key 进钥匙串、
/// env 变量名进注册表——上限取官方形态的宽裕倍数（最长官方 key 约 200 字符）
fn validate_field_lengths(
    env_var: Option<&str>,
    key: Option<&str>,
    base: Option<&str>,
) -> Result<(), String> {
    const ENV_VAR_MAX: usize = 128;
    const KEY_MAX: usize = 500;
    const BASE_MAX: usize = 300;
    if let Some(v) = env_var.filter(|s| !s.is_empty()) {
        if v.chars().count() > ENV_VAR_MAX {
            return Err(format!("环境变量名过长（上限 {ENV_VAR_MAX} 字符）"));
        }
    }
    if let Some(k) = key.filter(|s| !s.is_empty()) {
        if k.chars().count() > KEY_MAX {
            return Err(format!("API Key 过长（上限 {KEY_MAX} 字符）"));
        }
    }
    if let Some(b) = base.filter(|s| !s.is_empty()) {
        if b.chars().count() > BASE_MAX {
            return Err(format!("接口地址过长（上限 {BASE_MAX} 字符）"));
        }
    }
    Ok(())
}

/// 创建实例（M3-4 添加表单）。plain：真值只进钥匙串（失败报错引导改 env，D11）；
/// env：SQLite 存变量名。alias 全局唯一（2026-09-30 D14 升级，0009 索引），
/// 冲突 Err 透传给表单即时红字
#[tauri::command]
pub(crate) async fn provider_account_create(
    kind_id: String,
    alias: String,
    cred_mode: String,
    env_var: Option<String>,
    key: Option<String>,
    base_override: Option<String>,
    in_island: bool,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<String, String> {
    // async＋spawn_blocking（五轮审查）：本命令做全表读＋钥匙串写＋插库——
    // 凭据管理器偶发卡顿（域环境/凭据库大/Credential Manager 服务忙）时
    // 同步执行会冻结包括岛在内的全部窗口；与 provider_overview /
    // provider_account_test 同款（四轮只异步化了读路径，写路径漏掉）
    let store = store.inner().clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
    let alias = alias.trim().to_string();
    if alias.is_empty() {
        return Err("别名不能为空".into());
    }
    // 别名限长（2026-10-10 验收讨论）：前端 maxLength 同规则兜底——超长别名
    // 会撞 0009 唯一索引的键字节上限（约页大小 1/4），报错晦涩；上限 50 字符
    // 对别名绰绰有余（含 discovered 后缀最长约 12 字符）
    if alias.chars().count() > 50 {
        return Err("别名过长（上限 50 字符）".into());
    }
    validate_field_lengths(env_var.as_deref(), key.as_deref(), base_override.as_deref())?;
    if crate::provider::kind_of(&kind_id).is_none() {
        return Err(format!("未知厂商：{kind_id}"));
    }
    // 别名全局唯一预检（跨厂商也拦）：给友好文案而非透传 SQLite 报错；
    // 0009 唯一索引在库层兜底（桌面单用户，预检与落库间无实际竞态）
    if store
        .list_provider_accounts()
        .iter()
        .any(|a| a.alias == alias)
    {
        return Err(format!("别名已被其他实例使用：{alias}"));
    }
    let kind_log = kind_id.clone(); // env 分支会把 kind_id move 进实例，日志留一份
    // 本地推算厂商（M3-12 claude-local）：无凭据无端点，跳过凭据/base 必填直接建实例。
    // 单实例拦截（2026-09-29 审查修复 #15）：数据源是全局 agent='claude-code' 用量、
    // 档位是全局设置——多个实例只会产出完全相同的快照，多实例框架对这家名存实亡，
    // 创建时查重拦截（诚实报错优于重复数据）
    if crate::provider::kind_of(&kind_id).map(|k| k.local_only).unwrap_or(false) {
        if store
            .list_provider_accounts()
            .iter()
            .any(|a| a.kind_id == kind_id)
        {
            return Err("该厂商为本地推算口径，全局只有一份用量数据，无需重复添加实例".into());
        }
        let now = now_ms();
        let account = crate::store::ProviderAccount {
            id: uuid::Uuid::new_v4().to_string(),
            kind_id,
            alias: alias.clone(),
            base_override: None,
            cred_kind: "plain".into(),
            cred_value: None,
            note: None,
            enabled: true,
            in_island,
            origin: "manual".into(),
            created_at: now,
            updated_at: now,
        };
        store
            .insert_provider_account(&account)
            .map_err(|e| e.to_string())?;
        log::info!("[设置] 本地推算实例 {alias}（{kind_log}）已创建，id={}", account.id);
        return Ok(account.id);
    }
    // base 覆盖归一：去尾斜杠，空串视为用厂商默认
    let base_override = base_override
        .map(|b| b.trim().trim_end_matches('/').to_string())
        .filter(|b| !b.is_empty());
    // 自定义中转站无默认端点（M3-10）：base 必填，后端兜底校验（前端同规则）
    if kind_id == "custom-openai" && base_override.is_none() {
        return Err("自定义中转必须填写接口地址（base）".into());
    }
    let id = match cred_mode.as_str() {
        "env" => {
            let var = env_var.unwrap_or_default();
            let var = var.trim().to_string();
            if var.is_empty() {
                return Err("环境变量名不能为空".into());
            }
            let now = now_ms();
            let account = crate::store::ProviderAccount {
                id: uuid::Uuid::new_v4().to_string(),
                kind_id,
                alias: alias.clone(),
                base_override,
                cred_kind: "env".into(),
                cred_value: Some(var),
                note: None,
                enabled: true,
                in_island,
                origin: "manual".into(),
                created_at: now,
                updated_at: now,
            };
            store
                .insert_provider_account(&account)
                .map_err(|e| e.to_string())?;
            account.id
        }
        // plain（含未知值兜底）：走"先钥匙串再插库"公共底层，真值不落库
        _ => {
            let k = key.unwrap_or_default();
            if k.trim().is_empty() {
                return Err("API Key 不能为空".into());
            }
            crate::provider::bootstrap::insert_account_with_keyring(
                &store, &kind_id, &alias, base_override, "manual", None, k.trim(),
            )
            .map_err(|e| e.to_string())?
        }
    };
    log::info!("[设置] 实例 {alias}（{kind_log}）已创建，id={id}");
    Ok(id)
    })
    .await;
    match result {
        Ok(r) => r,
        Err(e) => {
            log::warn!("[设置] 实例创建任务失败：{e}");
            Err(format!("创建任务失败：{e}"))
        }
    }
}

/// 编辑实例（M3-4）。厂商锁定不可改（改厂商＝删了重建，拍板 2026-09-24）；
/// key=None 表示凭据不变更。凭据模式切换时同步钥匙串：
/// plain→env 删条目；env→plain 与 plain 换 key 需显式给出新 key 才写入
#[tauri::command]
pub(crate) async fn provider_account_update(
    id: String,
    alias: String,
    base_override: Option<String>,
    cred_mode: String,
    env_var: Option<String>,
    key: Option<String>,
    note: Option<String>,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<(), String> {
    // async＋spawn_blocking（五轮审查，同 create：钥匙串写＋全表读＋写库）
    let store = store.inner().clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
    let alias = alias.trim().to_string();
    if alias.is_empty() {
        return Err("别名不能为空".into());
    }
    // 别名限长（2026-10-10，同 create）：唯一索引键字节上限的友好兜底
    if alias.chars().count() > 50 {
        return Err("别名过长（上限 50 字符）".into());
    }
    validate_field_lengths(env_var.as_deref(), key.as_deref(), base_override.as_deref())?;
    let accounts = store.list_provider_accounts();
    let old = accounts
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| "实例不存在".to_string())?;
    // 别名全局唯一预检（2026-09-30 D14 升级；编辑排除自身，0009 唯一索引兜底）
    if accounts.iter().any(|a| a.id != id && a.alias == alias) {
        return Err(format!("别名已被其他实例使用：{alias}"));
    }
    let base_override = base_override
        .map(|b| b.trim().trim_end_matches('/').to_string())
        .filter(|b| !b.is_empty());
    // 自定义中转站 base 必填（M3-10）：编辑清空也拦截（后端兜底，前端同规则）
    if old.kind_id == "custom-openai" && base_override.is_none() {
        return Err("自定义中转必须填写接口地址（base）".into());
    }
    let (cred_kind, cred_value) = match cred_mode.as_str() {
        "env" => {
            let var = env_var.unwrap_or_default();
            let var = var.trim().to_string();
            if var.is_empty() {
                return Err("环境变量名不能为空".into());
            }
            ("env".to_string(), Some(var))
        }
        _ => ("plain".to_string(), None),
    };
    // 明文模式 + 显式新 key：覆盖写钥匙串（条目已存在时 set 即覆盖）
    if cred_kind == "plain" {
        if let Some(k) = key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty()) {
            keyring::Entry::new(crate::provider::KEYRING_SERVICE, &id)
                .and_then(|e| e.set_password(&k))
                .map_err(|e| format!("凭据写入钥匙串失败（可改用环境变量模式）：{e}"))?;
        }
    }
    // 旧明文 → 新环境变量：钥匙串条目删除（无条目幂等忽略）
    if old.cred_kind == "plain" && cred_kind == "env" {
        if let Ok(entry) = keyring::Entry::new(crate::provider::KEYRING_SERVICE, &id) {
            let _ = entry.delete_credential();
        }
    }
    store
        .update_provider_account(&id, &alias, base_override, &cred_kind, cred_value, note)
        .map_err(|e| e.to_string())?;
    log::info!("[设置] 实例 {alias}（{id}）已更新");
    Ok(())
    })
    .await;
    match result {
        Ok(r) => r,
        Err(e) => {
            log::warn!("[设置] 实例更新任务失败：{e}");
            Err(format!("更新任务失败：{e}"))
        }
    }
}

/// 删除实例（M3-4）：库行＋钥匙串条目一并删除（无条目幂等忽略）。
/// 历史快照保留（孤儿行仅作历史展示——拍板 2026-09-24，不破坏旧数据）
#[tauri::command]
pub(crate) async fn provider_account_delete(
    id: String,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<(), String> {
    // async＋spawn_blocking（五轮审查，同 create：钥匙串删＋删库行）
    let store = store.inner().clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
    store.delete_provider_account(&id).map_err(|e| e.to_string())?;
    if let Ok(entry) = keyring::Entry::new(crate::provider::KEYRING_SERVICE, &id) {
        let _ = entry.delete_credential();
    }
    log::info!("[设置] 实例 {id} 已删除（历史快照保留）");
    Ok(())
    })
    .await;
    match result {
        Ok(r) => r,
        Err(e) => {
            log::warn!("[设置] 实例删除任务失败：{e}");
            Err(format!("删除任务失败：{e}"))
        }
    }
}

/// 启停实例（停用不删除，凭据保留）。Result 透传（四轮审查）：失败让前端
/// 乐观 UI 回滚＋toast，不再静默
#[tauri::command]
pub(crate) fn provider_account_set_enabled(
    id: String,
    enabled: bool,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<(), String> {
    store
        .set_provider_account_enabled(&id, enabled)
        .map_err(|e| e.to_string())?;
    log::info!("[设置] 实例 {id} 已{}", if enabled { "启用" } else { "停用" });
    Ok(())
}

/// 岛展示集切换（软上限 5 由前端提示）。Result 透传（四轮审查，同启停）
#[tauri::command]
pub(crate) fn provider_account_set_in_island(
    id: String,
    in_island: bool,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<(), String> {
    store
        .set_provider_account_in_island(&id, in_island)
        .map_err(|e| e.to_string())
}

/// 实例卡片拖拽排序（设置页「额度与凭据」，2026-10-03）：按传入 id 顺序
/// 整体重写 sort_order，全实例唯一顺序源（岛轮播/托盘/额度页/报表曲线随之联动）。
/// Result 透传（同启停）：失败让前端乐观 UI 回滚＋toast
#[tauri::command]
pub(crate) fn provider_account_reorder(
    ids: Vec<String>,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<(), String> {
    store.reorder_provider_accounts(&ids).map_err(|e| e.to_string())?;
    log::info!("[设置] 实例排序已更新（共 {} 个）", ids.len());
    Ok(())
}

/// 一键复制完整 Key（2026-10-10，1Password「copy without reveal」同款）：
/// 钥匙串/env 解析真值 → 写系统剪贴板 → 120s 后读回比对、仍为本值则清空
/// （Windows 剪贴板历史 Win+V 会留存复制内容，延迟清空是泄露面收口的必要
/// 配套；比对防止误清用户 120s 内复制的其他内容）。真值不进前端，前端只收
/// 成功信号。async＋spawn_blocking：钥匙串读取可能卡顿（同 provider_overview）
#[tauri::command(async)]
pub(crate) async fn provider_account_copy_key(
    id: String,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<(), String> {
    const AUTO_CLEAR_SECS: u64 = 120;
    let store = store.inner().clone();
    let key = tauri::async_runtime::spawn_blocking(move || {
        let account = store
            .list_provider_accounts()
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| "实例不存在".to_string())?;
        if crate::provider::kind_of(&account.kind_id)
            .map(|k| k.local_only)
            .unwrap_or(false)
        {
            // 闭包统一 String 错误（与 resolve_creds 的 describe 口径一致）
            return Err("本地推算供应商无凭据可复制".to_string());
        }
        crate::provider::resolve_creds(&account)
            .map(|c| c.key)
            .map_err(|s| s.describe())
    })
    .await
    .map_err(|e| format!("复制任务失败：{e}"))??;
    if key.trim().is_empty() {
        return Err("该实例没有可复制的凭据".into());
    }
    copy_key_with_autoclear(&key, AUTO_CLEAR_SECS)?;
    log::info!("[凭据] 实例 Key 已复制到剪贴板（{AUTO_CLEAR_SECS}s 后自动清空）");
    Ok(())
}

/// 写剪贴板＋延迟清空（纯函数化的副作用封装）：120s 后读回剪贴板，仅当内容
/// 仍为本 Key 时覆写清空——期间用户复制了别的内容则不动。清空失败只留痕
/// 不报错（Key 已完成复制使命，主路径不受影响）
fn copy_key_with_autoclear(key: &str, secs: u64) -> Result<(), String> {
    let mut cb =
        arboard::Clipboard::new().map_err(|e| format!("剪贴板打开失败：{e}"))?;
    cb.set_text(key.to_string()).map_err(|e| format!("剪贴板写入失败：{e}"))?;
    let key = key.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(secs));
        let mut cb = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("[凭据] 剪贴板延迟清空打开失败（跳过）：{e}");
                return;
            }
        };
        match cb.get_text() {
            Ok(t) if t == key => {
                let _ = cb.clear();
                log::debug!("[凭据] 剪贴板中的 Key 已自动清空");
            }
            Ok(_) => {} // 剪贴板已被其他内容覆盖，不动
            Err(e) => log::warn!("[凭据] 剪贴板读回失败（跳过自动清空）：{e}"),
        }
    });
    Ok(())
}

/// 连接检测（M3-4 检测按钮/保存并检测共用）：凭据解析 → 工厂 → test()
/// （fetch 一次返回人类可读摘要）。async＋spawn_blocking：fetch 是阻塞外呼
/// （5s 超时），不能冻结窗口（run_report 同款理由）
#[tauri::command(async)]
pub(crate) async fn provider_account_test(
    id: String,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<String, String> {
    let store = store.inner().clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let account = store
            .list_provider_accounts()
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| "实例不存在".to_string())?;
        let creds = crate::provider::resolve_creds(&account).map_err(|s| s.describe())?;
        let adapter = crate::provider::build_adapter(&account, &creds, store.clone()).map_err(|e| e.to_string())?;
        adapter.test().map_err(|e| {
            // 双站变体（2026-10-10）：401/403 类失败附加站点上下文——双站 key
            // 互不通用（如 Moonshot 混用 401），用户需要知道当前请求的是哪个站
            let s = e.to_string();
            let host = crate::provider::host_of(creds.base.trim());
            if (s.contains("401") || s.contains("403")) && !host.is_empty() && !s.contains(host) {
                format!("{s}（当前站点：{host}；双站厂商的 Key 互不通用，请核对站点）")
            } else {
                s
            }
        })
    })
    .await;
    match result {
        Ok(r) => r,
        Err(e) => {
            log::warn!("[设置] 实例检测任务失败：{e}");
            Err(format!("检测任务失败：{e}"))
        }
    }
}

/// 单实例额度刷新（M3-5 额度页"单卡刷新"）：与检测不同——本命令把拉到的
/// 快照真正落库（与调度器同款 insert_quota/insert_balance），刷新后重拉
/// overview 才能看到数据与时间戳推进；检测（provider_account_test）仍保持
/// 只查询不落库的轻量语义，互不影响。5s 超时由适配器内部承担（红线③）
#[tauri::command]
pub(crate) async fn provider_account_refresh(
    id: String,
    store: tauri::State<'_, Arc<Store>>,
    notify: tauri::State<'_, QuotaRefreshNotify>,
) -> Result<String, String> {
    let store = store.inner().clone();
    let account_id = id.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let account = store
            .list_provider_accounts()
            .into_iter()
            .find(|a| a.id == account_id)
            .ok_or_else(|| "实例不存在".to_string())?;
        let creds = crate::provider::resolve_creds(&account).map_err(|s| s.describe())?;
        let adapter = crate::provider::build_adapter(&account, &creds, store.clone()).map_err(|e| e.to_string())?;
        // 落库口径与调度器一致：Windows 多条逐条入库，Balance 单条入库
        let snap = adapter.fetch().map_err(|e| e.to_string())?;
        let summary = crate::provider::describe_snapshot(&snap);
        match &snap {
            crate::provider::QuotaSnapshot::Windows(rows) => {
                for r in rows {
                    store.insert_quota(r);
                }
            }
            crate::provider::QuotaSnapshot::Balance(row) => store.insert_balance(row),
        }
        Ok(summary)
    })
    .await;
    match result {
        Ok(r) => {
            // 通知额度工作线程复位退避（通道满即丢无害：下轮到期至多多查一次）
            let _ = notify.0.try_send(id);
            r
        }
        Err(e) => {
            log::warn!("[额度] 实例刷新任务失败：{e}");
            Err(format!("刷新任务失败：{e}"))
        }
    }
}

/// env 变量就地解析检测（M3-4 表单 env 模式失焦反馈，D13 两层检测）：
/// Ok=脱敏描述；Err=分类文案（注册表命中未继承提示"重启应用后生效"）。
/// 脱敏统一走 crate::provider::mask_key（2026-10-10 审查：删除本地旧版
/// 前 3+****+后 4——8 字符 key 会暴露 7/8 字符；统一为三档口径）
#[tauri::command]
pub(crate) fn env_creds_probe(var: String) -> Result<String, String> {
    match crate::provider::env_key_lookup(var.trim()) {
        Ok(v) => Ok(crate::provider::mask_key(&v)),
        Err(s) => Err(s.describe()),
    }
}

/// hooks 安装状态（按 Agent 查询：配置文件中是否存在自家注入条目）。
/// #19 起三命令统一走 crate::collector::hooks_of 注册表（单点维护＋三函数防护一致）
#[tauri::command]
pub(crate) fn hooks_status(agent: String) -> Result<bool, String> {
    let ops = crate::collector::hooks_of(&agent).ok_or_else(|| format!("不支持的 Agent：{agent}"))?;
    Ok((ops.installed)())
}

/// 安装 hooks（增强档：精确状态）。async 标记：文件 IO 移出主线程，
/// 不阻塞事件循环（Tauri 语义，审查 2.2.1）
#[tauri::command(async)]
pub(crate) fn install_hooks(agent: String) -> Result<usize, String> {
    let ops = crate::collector::hooks_of(&agent).ok_or_else(|| format!("不支持的 Agent：{agent}"))?;
    // 失败留痕：配置文件被占用等失败原因只在错误链里，前端 toast 转瞬即逝
    (ops.install)().map_err(|e| {
        log::error!("[{agent}] hooks 注入失败：{e:#}");
        e.to_string()
    })
}

/// 卸载 hooks（还原配置文件）；async 标记理由同上
#[tauri::command(async)]
pub(crate) fn uninstall_hooks(agent: String) -> Result<usize, String> {
    let ops = crate::collector::hooks_of(&agent).ok_or_else(|| format!("不支持的 Agent：{agent}"))?;
    (ops.uninstall)().map_err(|e| {
        log::error!("[{agent}] hooks 卸载失败：{e:#}");
        e.to_string()
    })
}

/// 开机自启状态
#[tauri::command]
pub(crate) fn autostart_get(app: tauri::AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch()
        .is_enabled()
        .map_err(|e| e.to_string())
}

/// 设置开机自启（默认关，红线⑤）
#[tauri::command]
pub(crate) fn autostart_set(app: tauri::AppHandle, enable: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let al = app.autolaunch();
    let result = if enable { al.enable() } else { al.disable() };
    match result {
        // 低频关键事件走 info（同设置变更：事后排障不依赖开发者模式）
        Ok(()) => {
            log::info!("[设置] 开机自启已{}", if enable { "开启" } else { "关闭" });
            Ok(())
        }
        Err(e) => {
            log::warn!("[设置] 开机自启{}失败：{e}", if enable { "开启" } else { "关闭" });
            Err(e.to_string())
        }
    }
}

// ===== 关于窗口 commands =====

/// GitHub 仓库地址（与前端展示/复制文案保持同步：src/about/About.tsx 的 REPO_URL，两处同改）
pub(crate) const REPO_URL: &str = "https://github.com/ldtmore/AgentTrackerIsland";

/// 在系统默认浏览器打开项目仓库（关于页「GitHub 仓库」链接）。
/// URL 为 Rust 侧常量而非前端传参，零注入面（同 set_setting 白名单思路）；
/// explorer 打开 URL 即调起默认浏览器。升级策略（所有者拍板）：程序内不检测
/// 不下载不更新，由用户自行到 Releases 页下载安装包手动升级，本命令是唯一入口
#[tauri::command]
pub(crate) fn open_repository() -> Result<(), String> {
    match std::process::Command::new("explorer").arg(REPO_URL).spawn() {
        Ok(_) => Ok(()),
        Err(e) => {
            // 失败必须留痕（审查 1.1）：返回 Err 由前端提示，不 panic 不阻塞
            log::warn!("打开仓库链接失败：{e}");
            Err(format!("无法打开浏览器：{e}"))
        }
    }
}

// ===== 报表/会话窗口 commands（报表 M1-R1：整页快照；会话 M1-10：分页＋导出） =====
// 聚合查询是重查询（审查 2.2.1：Tauri 同步 command 在主线程执行，"全部"范围
// 大数据量时会冻结包括岛在内的全部窗口）——统一走 spawn_blocking 挪到线程池

/// 报表/会话查询公共壳：State 不能跨线程移动，先克隆 Arc 再进阻塞线程池。
/// 内层 Result 展平（查询失败与任务失败统一走 Err 通道）
pub(crate) async fn run_report<T>(
    store: tauri::State<'_, Arc<Store>>,
    query: impl FnOnce(&Store) -> Result<T, String> + Send + 'static,
) -> Result<T, String>
where
    T: Send + 'static,
{
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || query(&store))
        .await
        .map_err(|e| {
            log::warn!("[报表] 查询任务失败：{e}");
            format!("报表查询任务失败：{e}")
        })?
}

/// 报表：整页快照（范围档＋维度筛选一次返回全部图数据，图间口径一致）。
/// 范围档白名单见 store：today｜7d｜30d｜90d｜all
#[tauri::command]
pub(crate) async fn report_snapshot(
    range: String,
    agent: Option<Vec<String>>,
    project: Option<Vec<String>>,
    model: Option<Vec<String>>,
    provider: Option<Vec<String>>,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<crate::store::ReportSnapshot, String> {
    run_report(store, move |s| {
        s.report_snapshot(
            &range,
            agent.as_deref(),
            project.as_deref(),
            model.as_deref(),
            provider.as_deref(),
        )
        .ok_or_else(|| format!("未知范围档：{range}"))
    })
    .await
}

/// 会话中心：筛选下拉选项（轻查询，只随范围变化；维度筛选不叠加）
#[tauri::command]
pub(crate) async fn session_options(
    range: String,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<crate::store::FilterOptions, String> {
    run_report(store, move |s| {
        s.session_options(&range).ok_or_else(|| format!("未知范围档：{range}"))
    })
    .await
}

/// 会话中心：分页查询（M1-10，随会话窗口从报表迁移扩展）。
/// 范围档白名单见 store：today｜7d｜30d｜90d｜all；状态档 all｜active｜ended｜errored；
/// 排序键 recent｜tokens｜calls｜duration；page_size 每页行数（store 侧钳制 ≤200）
#[tauri::command]
pub(crate) async fn session_page(
    range: String,
    agent: Option<String>,
    project: Option<String>,
    model: Option<String>,
    status: String,
    keyword: String,
    sort: String,
    page_size: i64,
    offset: i64,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<crate::store::SessionPage, String> {
    run_report(store, move |s| {
        s.session_page(
            &range,
            agent.as_deref(),
            project.as_deref(),
            model.as_deref(),
            &status,
            &keyword,
            &sort,
            page_size,
            offset,
        )
        .ok_or_else(|| format!("未知范围档/状态档/排序键：{range}/{status}/{sort}"))
    })
    .await
}

/// 会话中心：会话详情（M1-11 抽屉）：单会话的调用流水 + 状态事件时间线
#[tauri::command]
pub(crate) async fn session_detail(
    session_id: String,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<crate::store::SessionDetail, String> {
    run_report(store, move |s| Ok(s.session_detail(&session_id))).await
}

/// 会话中心：导出当前范围＋筛选＋状态＋关键字＋排序的会话列表 CSV（所见即所得）。
/// 目标路径由前端保存对话框（tauri-plugin-dialog save）让用户自选，
/// 这里只负责生成内容并写入；后缀校验防误传任意路径
#[tauri::command]
pub(crate) async fn export_sessions_csv(
    range: String,
    agent: Option<String>,
    project: Option<String>,
    model: Option<String>,
    status: String,
    keyword: String,
    sort: String,
    path: String,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<String, String> {
    run_report(store, move |s| {
        if !path.to_ascii_lowercase().ends_with(".csv") {
            return Err("导出路径必须以 .csv 结尾".into());
        }
        let csv = s
            .build_sessions_csv(
                &range,
                agent.as_deref(),
                project.as_deref(),
                model.as_deref(),
                &status,
                &keyword,
                &sort,
            )
            .ok_or_else(|| format!("未知范围档/状态档/排序键：{range}/{status}/{sort}"))?;
        std::fs::write(&path, csv.as_bytes()).map_err(|e| format!("CSV 写入失败：{e}"))?;
        log::info!("[会话] 已导出 CSV：{}（{} 字节）", path, csv.len());
        Ok(path.clone())
    })
    .await
}

/// 导出报表 CSV（2026-09-29 审查修复 #11）：与 report_snapshot 同一次查询，
/// 导出与页面所见口径一致；结构＝范围/筛选快照＋汇总卡（含环比）＋六维度条
/// ＋趋势明细。保存对话框由前端负责，本命令只写文件
#[tauri::command(async)]
pub(crate) async fn export_report_csv(
    range: String,
    agent: Option<Vec<String>>,
    project: Option<Vec<String>>,
    model: Option<Vec<String>>,
    provider: Option<Vec<String>>,
    path: String,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<String, String> {
    run_report(store, move |s| {
        if !path.to_ascii_lowercase().ends_with(".csv") {
            return Err("导出路径必须以 .csv 结尾".into());
        }
        let csv = s
            .build_report_csv(
                &range,
                agent.as_deref(),
                project.as_deref(),
                model.as_deref(),
                provider.as_deref(),
            )
            .ok_or_else(|| format!("未知范围档：{range}"))?;
        std::fs::write(&path, csv.as_bytes()).map_err(|e| format!("CSV 写入失败：{e}"))?;
        log::info!("[报表] 已导出 CSV：{}（{} 字节）", path, csv.len());
        Ok(path.clone())
    })
    .await
}

/// 打开导出文件所在目录并选中该文件（导出提示文字的点击动作）。
/// 校验：文件必须真实存在且为 CSV——提示文字可能残留旧会话的路径，
/// 不允许拿它当任意 explorer 定位入口
#[tauri::command]
pub(crate) fn open_file_location(path: String) -> Result<(), String> {
    let p = std::path::Path::new(&path);
    if !p.is_file() || p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() != Some("csv") {
        return Err("文件不存在或不是 CSV".into());
    }
    // explorer /select,<路径>：打开资源管理器并定位选中；单参数拼接保证含空格路径不被拆散
    match std::process::Command::new("explorer").arg(format!("/select,{path}")).spawn() {
        Ok(_) => Ok(()),
        Err(e) => {
            log::warn!("[导出] 打开导出目录失败（{path}）：{e}");
            Err(format!("打开目录失败：{e}"))
        }
    }
}

/// 显示并聚焦报表窗口（2026-09-18 展示改造：岛面板汇总条的"报表"入口，
/// 与托盘菜单"报表"同一条路径；窗口为常驻隐藏窗口，只 show 不重建）。
/// 开窗前同样做屏幕高度钳制（与 show_aux_window 的报表分支同款，勿只改一处）
#[tauri::command]
pub(crate) fn show_report_window(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    match app.get_webview_window("report") {
        Some(w) => {
            clamp_report_height(&w);
            let _ = (w.show(), w.set_focus());
            Ok(())
        }
        None => {
            log::warn!("[报表] 窗口不存在（未初始化？），入口点击无效果");
            Err("报表窗口不可用".into())
        }
    }
}

/// 显示并聚焦会话窗口（M1-10：岛面板标题行/「查看更多会话」溢出链接与
/// 托盘菜单「会话」共用入口；常驻隐藏窗口，只 show 不重建）
#[tauri::command]
pub(crate) fn show_sessions_window(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    match app.get_webview_window("sessions") {
        Some(w) => {
            let _ = (w.show(), w.set_focus());
            Ok(())
        }
        None => {
            log::warn!("[会话] 窗口不存在（未初始化？），入口点击无效果");
            Err("会话窗口不可用".into())
        }
    }
}

/// 显示并聚焦额度窗口（M3-5：岛面板额度区/托盘额度行/空态引导共用入口；
/// 常驻隐藏窗口，只 show 不重建）。默认尺寸与报表同为 900×675，
/// 沿用同一份高度钳制（常量同源同值，勿只改一处）
#[tauri::command]
pub(crate) fn show_quotas_window(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    match app.get_webview_window("quotas") {
        Some(w) => {
            clamp_report_height(&w);
            let _ = (w.show(), w.set_focus());
            Ok(())
        }
        None => {
            log::warn!("[额度] 窗口不存在（未初始化？），入口点击无效果");
            Err("额度窗口不可用".into())
        }
    }
}

/// 显示并聚焦设置窗口（M3-5：额度页空态"去设置添加"直达入口；此前设置窗口
/// 仅托盘菜单可开，跨窗口动线补齐对称命令）
#[tauri::command]
pub(crate) fn show_settings_window(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    match app.get_webview_window("settings") {
        Some(w) => {
            let _ = (w.show(), w.set_focus());
            Ok(())
        }
        None => {
            log::warn!("[设置] 窗口不存在（未初始化？），入口点击无效果");
            Err("设置窗口不可用".into())
        }
    }
}

/// 打开程序日志/数据目录（07-UX 2.5 排障动线）：开发者模式日志与本地数据库
/// 均落在 %APPDATA%\com.agenttrackerisland.app 下（logs 子目录由 logging::init
/// 启动即建），从 app_data_dir 单一事实源定位。目录不存在（防御分支，理论不可
/// 达）返回 Err——前端 toast 报错不静默。与 open_file_location 分开实现：
/// 那是 CSV 导出定位专用（带扩展名白名单），语义不同不硬凑
#[tauri::command]
pub(crate) fn open_app_dir(app: tauri::AppHandle, kind: String) -> Result<(), String> {
    use tauri::Manager;
    let dir = match kind.as_str() {
        "log" => app.path().app_data_dir().map(|d| d.join("logs")),
        "data" => app.path().app_data_dir(),
        _ => return Err(format!("未知目录类型：{kind}")),
    }
    .map_err(|e| format!("定位应用目录失败：{e}"))?;
    if !dir.is_dir() {
        return Err(format!("目录不存在：{}", dir.display()));
    }
    // explorer 打开目录本身（不选中任何项），含空格路径单参数传递不会被拆散
    match std::process::Command::new("explorer").arg(&dir).spawn() {
        Ok(_) => {
            log::debug!("[排障] 打开目录（{kind}）：{}", dir.display());
            Ok(())
        }
        Err(e) => {
            log::warn!("[排障] 打开目录失败（{kind}）：{e}");
            Err(format!("打开目录失败：{e}"))
        }
    }
}

// ===== 贴边自动隐藏 commands（追加需求） =====
/// 报表最小高度（逻辑像素，与配置 minHeight 同源；钳制下限）
pub(crate) const REPORT_MIN_H: f64 = 520.0;

/// 显示并聚焦辅助窗口（设置/报表/额度/关于：常驻隐藏窗口，只 show 不重建）。
/// 报表/额度/会话窗口先做屏幕高度钳制（2026-09-24 所有者反馈：笔记本上默认高
/// 占满整屏；五轮审查批次三补入 sessions：其默认高 700 超过 720p 工作区 ≈672，
/// 首开底部压入任务栏）。07-UX 1.2 起钳制只在超出工作区容量时生效——
/// 窗口状态插件恢复的记忆尺寸放得下就保留，show 不再把用户尺寸打回默认
pub(crate) fn show_aux_window(app: &tauri::AppHandle, label: &str) {
    if let Some(w) = app.get_webview_window(label) {
        if label == "report" || label == "quotas" || label == "sessions" {
            clamp_report_height(&w);
        }
        let _ = (w.show(), w.set_focus());
    }
}

/// 报表高度钳制：外框高超过所在屏工作区可容纳高度时缩回去（绝不放大，
/// 不干扰用户手动调过的尺寸）；所在屏/工作区拿不到则保持配置默认，不阻塞显示。
/// 缩回后顶部位置不动可能探出屏幕底缘，顺带把纵向位置钳回工作区内
pub(crate) fn clamp_report_height(w: &tauri::WebviewWindow) {
    // 所在屏：显示过取 current；首开隐藏态可能拿不到，退主屏（笔记本即用户所指的屏）
    let mon = w
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| w.primary_monitor().ok().flatten());
    let Some(mon) = mon else { return };
    let scale = mon.scale_factor() as f64;
    let (wa_y, wa_h) = {
        let area = mon.work_area();
        (area.position.y, area.size.height as f64)
    };
    // 目标高度：工作区可容纳高度（减上下呼吸边），下限 minHeight。
    // 07-UX 1.2 起不再以默认高为上限——show_aux_window 会先经窗口状态插件
    // 恢复用户记忆尺寸，「超过默认 675 即缩回」会把记忆几何打回出生尺寸；
    // 超高缩回只服务「记忆于大屏 → 换小屏/改缩放后放不下」场景
    let target = (((wa_h - 48.0).max(0.0)) as u32).max((REPORT_MIN_H * scale) as u32);
    let Ok(cur) = w.outer_size() else { return };
    if cur.height <= target {
        return; // 放得下（含用户手动调小过）→ 不动
    }
    let _ = w.set_size(tauri::PhysicalSize::new(cur.width, target));
    if let Ok(pos) = w.outer_position() {
        let min_y = wa_y;
        let max_y = (wa_y + wa_h as i32 - target as i32).max(min_y);
        let y = pos.y.clamp(min_y, max_y);
        if y != pos.y {
            let _ = w.set_position(tauri::PhysicalPosition::new(pos.x, y));
        }
    }
}

/// 启动时对五个辅助窗口做一次工作区钳制（07-UX 1.2 出屏兜底）：窗口状态插件
/// 恢复几何时，仅当保存位置与所有现存显示器都不相交才放弃恢复——「保存于
/// 外接屏 → 拔屏 → 重启」落在现存屏边缘半出屏（相交）的场景会原样恢复出屏。
/// 这里显式把位置钳回所在屏工作区、尺寸钳到不超工作区；拔屏后保存坐标与
/// 所有显示器都不相交时插件已自行回落 OS 定位，本函数对合法位置是 no-op。
/// 隐藏窗口的 set_position/set_size 照常生效；已最大化（记忆位）不动——
/// 对最大化窗口 set_position 会解除最大化状态。
/// 锁纪律/panic 纪律：setup 内同步执行，i32::clamp 上界恒 ≥ 下界（尺寸先钳
/// 工作区），不存在窄屏 min>max panic 面（与 island::safe_clamp 同思路）
pub(crate) fn clamp_aux_windows_on_boot(app: &tauri::AppHandle) {
    for label in ["settings", "report", "quotas", "sessions", "about"] {
        let Some(w) = app.get_webview_window(label) else { continue };
        if w.is_maximized().unwrap_or(false) {
            continue;
        }
        // 所在屏：恢复后窗口已有坐标，current_monitor 按位置解析；
        // 拿不到退主屏（与 clamp_report_height 同款兜底）
        let mon = w
            .current_monitor()
            .ok()
            .flatten()
            .or_else(|| w.primary_monitor().ok().flatten());
        let Some(mon) = mon else { continue };
        let (Ok(size), Ok(pos)) = (w.outer_size(), w.outer_position()) else {
            continue;
        };
        let area = mon.work_area();
        let (wa_x, wa_y) = (area.position.x, area.position.y);
        let (wa_w, wa_h) = (area.size.width as i32, area.size.height as i32);
        // 先钳尺寸不超工作区（改缩放后旧尺寸可能超出；PhysicalSize 为 u32、
        // 工作区为 i32，参与位置运算时显式收窄转换），再钳位置进工作区
        let new_w = size.width.min(wa_w as u32);
        let new_h = size.height.min(wa_h as u32);
        let new_x = pos.x.clamp(wa_x, (wa_x + wa_w - new_w as i32).max(wa_x));
        let new_y = pos.y.clamp(wa_y, (wa_y + wa_h - new_h as i32).max(wa_y));
        if new_w != size.width || new_h != size.height {
            let _ = w.set_size(tauri::PhysicalSize::new(new_w, new_h));
        }
        if new_x != pos.x || new_y != pos.y {
            let _ = w.set_position(tauri::PhysicalPosition::new(new_x, new_y));
        }
    }
}

