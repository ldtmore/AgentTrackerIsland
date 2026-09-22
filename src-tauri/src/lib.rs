// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
pub mod commands;
pub mod collector;
pub mod logging;
pub mod provider;
pub mod state;
pub mod store;

use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tauri::{Emitter, Manager};

use crate::state::service::Aggregator;
use crate::store::Store;

/// 岛窗口标签（tauri.conf.json 中定义）
const ISLAND: &str = "island";

// ===== 贴边自动隐藏（追加需求：自由拖拽 + 贴边隐藏，默认开启） =====

/// 岛收缩态固定高度（逻辑像素）；宽度按屏幕自适应，见 island_width()
const ISLAND_H: i32 = 48;
/// 岛展开面板高度（逻辑像素）
const ISLAND_EXPANDED_H: i32 = 520;
/// 自适应宽度：显示器逻辑宽 × 比例，夹取 [MIN， MAX]（小屏保底、大屏封顶）。
/// 比例取 30%：介于三分律（1/3）与黄金分割小段（0.382）之间、主流悬浮组件
/// 25%~35% 区间的中值（NN/g、Figma 设计参考；所有者笔记本实测 +1/5 手感吻合）
const ISLAND_W_RATIO: f64 = 0.30;
const ISLAND_W_MIN: i32 = 380;
const ISLAND_W_MAX: i32 = 800;
/// 顶部贴边隐藏的露出高度（≈胶囊 48 的 1/5 略加余量：胶囊底部的独立信息条）
const PEEK_TOP_H: i32 = 14;
/// 左右贴边隐藏的伸出宽度（半圆 D 形标签）
const PEEK_SIDE_W: i32 = 20;
/// 贴靠判定阈值（逻辑像素）：拖放位置距屏幕边小于该值即吸附到该边
const SNAP_THRESHOLD: i32 = 24;
/// 拖拽防抖：Moved 事件静默该时长且左键已释放，才认定"拖放完成"并评估贴靠
const DRAG_QUIET_MS: u128 = 180;

/// 岛自适应宽度：显示器逻辑宽 × 30%，夹取 [380， 800]。
/// 1366→410 / 1440→432 / 1920→576 / 2560→768 / 3840→800；Rust 贴边几何与前端渲染共用
fn island_width(mon_logical_w: i32) -> i32 {
    ((mon_logical_w as f64 * ISLAND_W_RATIO).round() as i32)
        .clamp(ISLAND_W_MIN, ISLAND_W_MAX)
}

/// 顶部贴边隐藏态宽度：胶囊公式的常数全部减半（比例/上下限各取 1/2），
/// 与胶囊恒保持 2:1——任意屏幕上"窄标签 → 全宽胶囊"的生长比例一致。
/// 1366→205 / 1920→288 / 2560→384 / ≥2667→400；经 island_metrics 下发前端渲染
fn peek_top_width(mon_logical_w: i32) -> i32 {
    ((mon_logical_w as f64 * ISLAND_W_RATIO * 0.5).round() as i32)
        .clamp(ISLAND_W_MIN / 2, ISLAND_W_MAX / 2)
}

/// 岛的运动/贴边状态（内存态，setup 时创建并全局共享；位置与开关持久化在 app_settings）
#[derive(Default)]
struct IslandMotion {
    /// 拖拽中的待评估位置（事件时间戳 + 坐标），看护线程防抖后消费
    pending: Option<(std::time::Instant, i32, i32)>,
    /// 程序化滑动（吸附/隐藏/显示）的目标落点：用于识别并消费动画自己产生的 Moved 事件
    programmed: Option<(i32, i32)>,
    /// 滑动动画进行中：开始时刻 + 落点。Moved 事件消费落点即结束；
    /// 落点事件意外丢失时按超时自复位，防"动画标记永久卡死"（审查 3.7:
    /// 由独立的兜底线程改为时间戳判定，少一个短命线程）
    animating: Option<(std::time::Instant, (i32, i32))>,
    /// 当前贴靠边："none" | "top" | "left" | "right"
    edge: String,
    /// 是否处于滑出隐藏态
    hidden: bool,
}

/// 动画标记超时：超过该时长仍未等到落点 Moved 事件则自复位（正常动画约 160~240ms）
const ANIM_TIMEOUT_MS: u128 = 400;

/// 滑动动画代数：每次新滑动/用户拖拽都递增，使旧动画线程自行退出
static SLIDE_GEN: AtomicU64 = AtomicU64::new(0);

/// 点击会话卡片 → 激活对应终端/IDE 窗口（T10，窗口级定位）
#[tauri::command]
fn focus_session(session_id: String, store: tauri::State<'_, Arc<Store>>) -> bool {
    let Some((agent, project_dir)) = store.get_session_meta(&session_id) else {
        // 点击跳转无反应的根因之一：会话不在自库（扫描截断/未采集到）
        log::debug!("[跳转] 会话元数据缺失（库里查不到）：{session_id}");
        return false;
    };
    let ok = commands::find_session_window(&agent, project_dir.as_deref())
        .map(commands::activate_window)
        .unwrap_or(false);
    // 窗口找到了但激活失败（SetForegroundWindow 可能被系统拒绝）单独留痕
    if !ok {
        log::debug!("[跳转] 窗口已找到但激活失败：agent={agent} session={session_id}");
    }
    ok
}

// ===== 前端日志通道（2026-09-17 埋点审查 P2） =====

/// 前端日志级别白名单（防任意字符串透传）
const FRONTEND_LEVELS: &[&str] = &["error", "warn", "info", "debug"];

/// 前端（webview）日志落文件：全局 onerror / unhandledrejection / ErrorBoundary
/// 与关键交互手动埋点统一走这里。窗口名取自 Tauri 窗口 label（前端不用传），
/// target 固定 frontend，与 Rust 侧日志同一文件同一格式。
#[tauri::command]
fn log_frontend(win: tauri::WebviewWindow, level: String, message: String) {
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
/// （如覆盖 hook_events_offset/island_pos 等内部状态键）
const SETTING_KEYS_ALLOW: &[&str] = &[
    "glm_base",
    "glm_token",
    "threshold_warn",
    "threshold_danger",
    "cleanup_days",
    "island_autohide",
    "island_opacity",
    "hover_expand",
    "tray_left_action",
    "agents_enabled",
    "agent_colors",
    "theme",
    "dev_mode",
];

/// 读取全部设置。
/// glm_token 原样随设置下发（2026-09-17 所有者要求 API Key 回显输入框，
/// 推翻原审查 2.1.2"敏感值不下发前端"的决策；仅下发到本机自身窗口）
#[tauri::command]
fn get_settings(store: tauri::State<'_, Arc<Store>>) -> std::collections::HashMap<String, String> {
    store.all_settings()
}

/// 写单条设置（白名单外的键拒绝并报错，前端会显示"保存失败"）
#[tauri::command]
fn set_setting(key: String, value: String, store: tauri::State<'_, Arc<Store>>) -> Result<(), String> {
    if !SETTING_KEYS_ALLOW.contains(&key.as_str()) {
        log::warn!("拒绝写入未登记的设置键：{key}");
        return Err(format!("不允许写入设置键：{key}"));
    }
    store.set_setting(&key, &value);
    // 设置变更留痕（info：低频关键事件，事后排障不依赖用户提前开开发者模式；
    // API Key 绝不落明文——日志文件会被用户分享出去，只记长度）
    if key == "glm_token" {
        log::info!("[设置] glm_token 已更新（长度 {} 字符）", value.len());
    } else {
        log::info!("[设置] {key} = {value}");
    }
    // 开发者模式即时切换日志级别（免重启）
    if key == "dev_mode" {
        logging::set_verbose(value == "1");
    }
    Ok(())
}

/// 支持 hooks 增强档的 Agent 清单（M2-6/7/11）：五家各有独立注入器与事件文件。
/// 前端按此渲染 hooks 卡片，命令按 agent 参数化分发
const HOOKS_AGENTS: &[&str] = &["claude-code", "codex", "kimi-code", "gemini", "qwen-code"];

/// hooks 安装状态（按 Agent 查询：配置文件中是否存在自家注入条目）
#[tauri::command]
fn hooks_status(agent: String) -> Result<bool, String> {
    if !HOOKS_AGENTS.contains(&agent.as_str()) {
        return Err(format!("不支持的 Agent：{agent}"));
    }
    Ok(match agent.as_str() {
        "codex" => collector::codex::hooks_installed(),
        "kimi-code" => collector::kimi::hooks_installed(),
        "gemini" => collector::gemini::hooks_installed(),
        "qwen-code" => collector::qwen::hooks_installed(),
        _ => collector::claude_code::hooks_installed(),
    })
}

/// 安装 hooks（增强档：精确状态）。async 标记：文件 IO 移出主线程，
/// 不阻塞事件循环（Tauri 语义，审查 2.2.1）
#[tauri::command(async)]
fn install_hooks(agent: String) -> Result<usize, String> {
    let result = match agent.as_str() {
        "codex" => collector::codex::install_hooks(),
        "kimi-code" => collector::kimi::install_hooks(),
        "gemini" => collector::gemini::install_hooks(),
        "qwen-code" => collector::qwen::install_hooks(),
        "claude-code" => collector::claude_code::install_hooks(),
        other => Err(anyhow::anyhow!("不支持的 Agent：{other}")),
    };
    // 失败留痕：配置文件被占用等失败原因只在错误链里，前端 toast 转瞬即逝
    result.map_err(|e| {
        log::error!("[{agent}] hooks 注入失败：{e:#}");
        e.to_string()
    })
}

/// 卸载 hooks（还原配置文件）；async 标记理由同上
#[tauri::command(async)]
fn uninstall_hooks(agent: String) -> Result<usize, String> {
    let result = match agent.as_str() {
        "codex" => collector::codex::uninstall_hooks(),
        "kimi-code" => collector::kimi::uninstall_hooks(),
        "gemini" => collector::gemini::uninstall_hooks(),
        "qwen-code" => collector::qwen::uninstall_hooks(),
        "claude-code" => collector::claude_code::uninstall_hooks(),
        other => Err(anyhow::anyhow!("不支持的 Agent：{other}")),
    };
    result.map_err(|e| {
        log::error!("[{agent}] hooks 卸载失败：{e:#}");
        e.to_string()
    })
}

/// 开机自启状态
#[tauri::command]
fn autostart_get(app: tauri::AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch()
        .is_enabled()
        .map_err(|e| e.to_string())
}

/// 设置开机自启（默认关，红线⑤）
#[tauri::command]
fn autostart_set(app: tauri::AppHandle, enable: bool) -> Result<(), String> {
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
const REPO_URL: &str = "https://github.com/ldtmore/AgentTrackerIsland";

/// 在系统默认浏览器打开项目仓库（关于页「GitHub 仓库」链接）。
/// URL 为 Rust 侧常量而非前端传参，零注入面（同 set_setting 白名单思路）；
/// explorer 打开 URL 即调起默认浏览器。升级策略（所有者拍板）：程序内不检测
/// 不下载不更新，由用户自行到 Releases 页下载安装包手动升级，本命令是唯一入口
#[tauri::command]
fn open_repository() -> Result<(), String> {
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
async fn run_report<T>(
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
async fn report_snapshot(
    range: String,
    agent: Option<String>,
    project: Option<String>,
    model: Option<String>,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<store::ReportSnapshot, String> {
    run_report(store, move |s| {
        s.report_snapshot(&range, agent.as_deref(), project.as_deref(), model.as_deref())
            .ok_or_else(|| format!("未知范围档：{range}"))
    })
    .await
}

/// 会话中心：筛选下拉选项（轻查询，只随范围变化；维度筛选不叠加）
#[tauri::command]
async fn session_options(
    range: String,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<store::FilterOptions, String> {
    run_report(store, move |s| {
        s.session_options(&range).ok_or_else(|| format!("未知范围档：{range}"))
    })
    .await
}

/// 会话中心：分页查询（M1-10，随会话窗口从报表迁移扩展）。
/// 范围档白名单见 store：today｜7d｜30d｜90d｜all；状态档 all｜active｜ended｜errored；
/// 排序键 recent｜tokens｜calls｜duration；page_size 每页行数（store 侧钳制 ≤200）
#[tauri::command]
async fn session_page(
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
) -> Result<store::SessionPage, String> {
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
async fn session_detail(
    session_id: String,
    store: tauri::State<'_, Arc<Store>>,
) -> Result<store::SessionDetail, String> {
    run_report(store, move |s| Ok(s.session_detail(&session_id))).await
}

/// 会话中心：导出当前范围＋筛选＋状态＋关键字＋排序的会话列表 CSV（所见即所得）。
/// 目标路径由前端保存对话框（tauri-plugin-dialog save）让用户自选，
/// 这里只负责生成内容并写入；后缀校验防误传任意路径
#[tauri::command]
async fn export_sessions_csv(
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

/// 打开导出文件所在目录并选中该文件（导出提示文字的点击动作）。
/// 校验：文件必须真实存在且为 CSV——提示文字可能残留旧会话的路径，
/// 不允许拿它当任意 explorer 定位入口
#[tauri::command]
fn open_file_location(path: String) -> Result<(), String> {
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
/// 与托盘菜单"报表"同一条路径；窗口为常驻隐藏窗口，只 show 不重建）
#[tauri::command]
fn show_report_window(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    match app.get_webview_window("report") {
        Some(w) => {
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
fn show_sessions_window(app: tauri::AppHandle) -> Result<(), String> {
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

// ===== 贴边自动隐藏 commands（追加需求） =====

/// 岛自适应尺寸（前端挂载时获取，Rust 贴边几何与前端渲染共用同一公式）
#[derive(serde::Serialize)]
struct IslandMetrics {
    width: i32,
    collapsed_h: i32,
    expanded_h: i32,
    /// 顶部贴边隐藏态宽度（peek_top_width：胶囊常数减半，恒 2:1）
    peek_top_w: i32,
}

/// 查询岛自适应尺寸（基于岛窗口当前所在显示器的逻辑宽度）
#[tauri::command]
fn island_metrics(win: tauri::WebviewWindow) -> Option<IslandMetrics> {
    let mon = win.current_monitor().ok().flatten()?;
    let ml = monitor_logical(&mon);
    Some(IslandMetrics {
        width: island_width(ml.2),
        collapsed_h: ISLAND_H,
        expanded_h: ISLAND_EXPANDED_H,
        peek_top_w: peek_top_width(ml.2),
    })
}

/// 鼠标移入贴边岛 → 滑入显示（show=true）/移出 → 滑出隐藏（show=false）。
/// 未贴边（edge=none）或关闭自动隐藏时为无害 no-op
#[tauri::command]
fn island_peek(
    show: bool,
    app: tauri::AppHandle,
    store: tauri::State<'_, Arc<Store>>,
    motion: tauri::State<'_, Arc<Mutex<IslandMotion>>>,
) {
    let (edge, hidden) = {
        let m = motion.lock().unwrap();
        (m.edge.clone(), m.hidden)
    };
    if edge == "none" {
        return;
    }
    // 目标态推导：滑入仅当当前隐藏；滑出仅当当前可见且自动隐藏开启（幂等守卫）
    let target = match (show, hidden) {
        (true, true) => Some(IslandTarget::Pill { summon: false }),
        (false, false) if autohide_enabled(store.as_ref()) => {
            Some(IslandTarget::Peek { jump: false })
        }
        _ => None,
    };
    if let Some(t) = target {
        island_transition(&app, t);
    }
}

/// 贴边相关设置变更后的状态修正（双向对称）：
/// - 关闭自动隐藏时岛正处于隐藏态 → 滑回停靠位显示
/// - 开启自动隐藏时岛正停靠可见 → 立即滑出隐藏
#[tauri::command]
fn island_refresh(
    app: tauri::AppHandle,
    store: tauri::State<'_, Arc<Store>>,
    motion: tauri::State<'_, Arc<Mutex<IslandMotion>>>,
) {
    let (edge, hidden) = {
        let m = motion.lock().unwrap();
        (m.edge.clone(), m.hidden)
    };
    if edge != "none" {
        // 双向对称修正：关自动隐藏时若在隐藏态 → 滑回；开自动隐藏时若在停靠态 → 滑出
        let target = match (hidden, autohide_enabled(store.as_ref())) {
            (true, false) => Some(IslandTarget::Pill { summon: false }),
            (false, true) => Some(IslandTarget::Peek { jump: false }),
            _ => None,
        };
        if let Some(t) = target {
            island_transition(&app, t);
        }
    }
}

/// 查询岛当前贴边/隐藏状态（前端挂载时主动拉取一次：启动恢复发生在 setup 阶段，
/// 早于前端事件监听建立，事件推送会漏掉首帧，导致重启后的隐藏态渲染成完整胶囊）
#[tauri::command]
fn island_dock_state(motion: tauri::State<'_, Arc<Mutex<IslandMotion>>>) -> serde_json::Value {
    let m = motion.lock().unwrap();
    serde_json::json!({ "edge": m.edge, "hidden": m.hidden })
}

/// 用户按下岛（拖拽开始）：取消在播滑动动画与待评估位置，
/// 避免拖拽循环和滑动动画互相抢窗口（程序化 set_position 会打断系统拖拽）
#[tauri::command]
fn island_drag_start(motion: tauri::State<'_, Arc<Mutex<IslandMotion>>>) {
    SLIDE_GEN.fetch_add(1, Ordering::Relaxed);
    let mut m = motion.lock().unwrap();
    m.animating = None;
    m.programmed = None;
    m.pending = None;
}

/// 贴边自动隐藏开关（设置项 island_autohide，缺省=开）
fn autohide_enabled(store: &Store) -> bool {
    store
        .get_setting("island_autohide")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// 解析 island_pos 设置（"x,y" 逻辑坐标；写入方 apply_snap 持久化的是逻辑坐标）
fn saved_pos(store: &Store) -> Option<(i32, i32)> {
    store.get_setting("island_pos").and_then(|s| {
        s.split_once(',')
            .and_then(|(a, b)| match (a.trim().parse::<i32>(), b.trim().parse::<i32>()) {
                (Ok(x), Ok(y)) => Some((x, y)),
                _ => None,
            })
    })
}

/// 物理坐标 → 逻辑坐标（按显示器缩放换算）
fn phys_to_logical(v: (i32, i32), scale: f64) -> (i32, i32) {
    (
        (v.0 as f64 / scale).round() as i32,
        (v.1 as f64 / scale).round() as i32,
    )
}

/// 逻辑坐标 → 物理坐标
fn logical_to_phys(v: (i32, i32), scale: f64) -> (i32, i32) {
    (
        (v.0 as f64 * scale).round() as i32,
        (v.1 as f64 * scale).round() as i32,
    )
}

/// 显示器矩形（逻辑坐标）：x， y， w， h
fn monitor_logical(mon: &tauri::Monitor) -> (i32, i32, i32, i32) {
    let s = mon.scale_factor();
    let mp = mon.position();
    (
        (mp.x as f64 / s).round() as i32,
        (mp.y as f64 / s).round() as i32,
        (mon.size().width as f64 / s).round() as i32,
        (mon.size().height as f64 / s).round() as i32,
    )
}

/// 判定拖放位置应吸附的屏幕边（优先级：上 > 左 > 右；越界超出阈值视为自由位置）。
/// mon = (x， y， w， h) 显示器矩形；独立函数便于几何单测
fn detect_edge(
    pos: (i32, i32),
    mon: (i32, i32, i32, i32),
    size: (i32, i32),
    threshold: i32,
) -> &'static str {
    let (mx, my, mw, _) = mon;
    let (x, y) = pos;
    if y - my <= threshold {
        "top"
    } else if x - mx <= threshold {
        "left"
    } else if mx + mw - (x + size.0) <= threshold {
        "right"
    } else {
        "none"
    }
}

/// 隐藏位坐标：按贴靠边把窗口滑出屏幕，仅留露出常量对应的信息条/半圆标签
/// （显示器以纯数值传入：mon_pos=原点，mon_w=宽度；独立函数便于几何单测）
fn hidden_pos(
    mon_pos: (i32, i32),
    mon_w: i32,
    edge: &str,
    docked: (i32, i32),
    island_w: i32,
) -> (i32, i32) {
    match edge {
        "top" => (docked.0, mon_pos.1 - (ISLAND_H - PEEK_TOP_H)),
        "left" => (mon_pos.0 - (island_w - PEEK_SIDE_W), docked.1),
        "right" => (mon_pos.0 + mon_w - PEEK_SIDE_W, docked.1),
        _ => docked,
    }
}

/// 滑动模式：Jump = 瞬时跳变（启动定位/启动恢复）；Out/In = 时长化缓动（毫秒）。
/// 显示/吸附用 Out（ease-out cubic，减速进场），隐藏用 In（ease-in cubic，加速离场）——
/// 不对称缓动：进场从容、离场干脆，观感更精致
enum Slide {
    Jump,
    Out(u64),
    In(u64),
}

/// 滑入（显示/吸附落位）时长：减速进场
const SLIDE_SHOW_MS: u64 = 220;
/// 滑出（隐藏）时长：加速离场
const SLIDE_HIDE_MS: u64 = 160;

/// 程序化滑动窗口到位（to 为逻辑坐标）。动画期间产生的 Moved 事件由
/// IslandMotion.animating 屏蔽，落点由 programmed 消费，防止"移动 → Moved →
/// 再评估 → 再移动"的自触发循环；落点事件意外丢失时由 Moved 处理器按超时
/// 自复位（审查 3.7：去掉独立兜底线程）。按 16ms 步进插值缓动曲线，
/// 末步恰为落点（缓动函数 f(1)=1），保证 Moved 落点消费判定不受缓动影响
fn slide_to(
    win: &tauri::WebviewWindow,
    to: (i32, i32),
    scale: f64,
    motion: &Arc<Mutex<IslandMotion>>,
    anim: Slide,
) {
    let Ok(from_phys) = win.outer_position() else {
        return;
    };
    let from = phys_to_logical((from_phys.x, from_phys.y), scale);
    if from == to {
        return;
    }
    let to_phys = logical_to_phys(to, scale);
    let gen = SLIDE_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    {
        let mut m = motion.lock().unwrap();
        m.animating = Some((std::time::Instant::now(), to_phys));
        m.programmed = Some(to_phys);
    }
    // 时长 → 步数（16ms 一步，至少 2 步保证缓动曲线有中间帧；Jump 恒 1 步线性直达）
    let (steps, dur_ms, ease_in) = match anim {
        Slide::Jump => (1u64, 0u64, false),
        Slide::Out(ms) => ((ms / 16).max(2), ms, false),
        Slide::In(ms) => ((ms / 16).max(2), ms, true),
    };
    let win = win.clone();
    std::thread::spawn(move || {
        for i in 1..=steps {
            if SLIDE_GEN.load(Ordering::Relaxed) != gen {
                return; // 被更新的滑动/用户拖拽取代
            }
            // 缓动插值：ease-in cubic（t³，起步慢加速离场）/ ease-out cubic（1−(1−t)³，进场减速）
            let t = i as f64 / steps as f64;
            let e = if ease_in { t * t * t } else { 1.0 - (1.0 - t).powi(3) };
            let lx = from.0 + ((to.0 - from.0) as f64 * e).round() as i32;
            let ly = from.1 + ((to.1 - from.1) as f64 * e).round() as i32;
            let phys = logical_to_phys((lx, ly), scale);
            let _ = win.set_position(tauri::PhysicalPosition::new(phys.0, phys.1));
            if i < steps && dur_ms > 0 {
                std::thread::sleep(Duration::from_millis(dur_ms / steps as u64));
            }
        }
    });
}

/// 岛的目标状态：显隐语义的唯一来源。全项目任何入口（托盘、悬停、设置页开关、
/// 拖放吸附、启动恢复）都只声明目标态，编排细节一律由 island_transition 执行——
/// 新增显隐入口不再各自写一套时序（2026-09-20 M1-9 显隐逻辑统一改造）
enum IslandTarget {
    /// 整窗隐藏（托盘隐藏）；停靠位仍记忆，供下次召回
    Gone,
    /// 贴边滑出、露标签。jump=true 瞬移（启动恢复专用，无动画）
    Peek { jump: bool },
    /// 胶囊可见（贴边滑回停靠位，或自由位原地亮出）。summon=true 时通知前端
    /// 3s 无操作自动收回（临时召唤语义）
    Pill { summon: bool },
}

/// 岛显隐唯一转换函数：全项目只允许这里动岛的窗口几何/可见性。
/// 编排顺序固定——①改状态 ②窗口几何（收拢/滑动）③发 island-dock 事件
/// ④窗口 show ⑤附带动作（穿透/召唤）。关键不变量：Gone 先预切前端 DOM 为
/// 胶囊再整窗隐藏（隐藏期间 DOM 不可见，切换零成本），此后任何 show 的首帧
/// 必然是目标态——「亮出旧状态残影再跳变」从机制上根除；show 与事件是异步
/// 竞速，仅靠调用顺序压不住（2026-09-20 托盘召唤残影实测教训）。
/// ⚠ 历史教训（2026-09-18）：slide_to 内部会 lock motion，本函数全程不得持
/// motion 锁调用它——MutexGuard 活到作用域外会同线程自锁、全 UI 冻结
fn island_transition(app: &tauri::AppHandle, target: IslandTarget) {
    let Some(win) = app.get_webview_window(ISLAND) else {
        return;
    };
    let motion = app.state::<Arc<Mutex<IslandMotion>>>();
    let edge = motion.lock().unwrap().edge.clone();

    match target {
        IslandTarget::Gone => {
            // ① DOM 预切回胶囊态（edge=none 时前端本就渲染胶囊，事件幂等）
            // ② 整窗隐藏；穿透状态交由看护线程在下次显示前自然收敛
            let _ = app.emit_to(
                ISLAND,
                "island-dock",
                serde_json::json!({ "edge": edge, "hidden": false }),
            );
            if let Err(e) = win.hide() {
                log::debug!("[岛] 转换 Gone 隐藏失败：{e}");
            }
            log::debug!("[岛] 转换 Gone（DOM 已预切胶囊）");
        }
        IslandTarget::Peek { jump } => {
            // 非贴边停靠不存在"滑出隐藏"语义（island_peek/island_refresh 已守卫，
            // 此处再防御一次）；停靠坐标缺失同样落空
            if edge == "none" {
                return;
            }
            let store = app.state::<Arc<Store>>();
            let Some(docked) = saved_pos(&store) else {
                return;
            };
            let Ok(Some(mon)) = win.current_monitor() else {
                return;
            };
            let ml = monitor_logical(&mon);
            let width = island_width(ml.2);
            // 面板展开时贴边隐藏：先收拢到胶囊尺寸，锚定窗口底部的标签才会贴住屏幕边
            let _ = win.set_size(tauri::LogicalSize::new(width, ISLAND_H));
            {
                let mut m = motion.lock().unwrap();
                m.hidden = true;
            }
            // 滑出隐藏（In：加速离场）；启动恢复走 Jump 瞬移。
            // 穿透交由看护线程按光标位置接管（90ms 节拍）
            slide_to(
                &win,
                hidden_pos((ml.0, ml.1), ml.2, &edge, docked, width),
                mon.scale_factor(),
                &motion,
                if jump { Slide::Jump } else { Slide::In(SLIDE_HIDE_MS) },
            );
            let _ = app.emit_to(
                ISLAND,
                "island-dock",
                serde_json::json!({ "edge": edge, "hidden": true }),
            );
            log::debug!("[岛] 转换 Peek：edge={edge}");
        }
        IslandTarget::Pill { summon } => {
            let Ok(Some(mon)) = win.current_monitor() else {
                return;
            };
            let ml = monitor_logical(&mon);
            let width = island_width(ml.2);
            // 面板可能处于展开态，先收拢到胶囊尺寸
            let _ = win.set_size(tauri::LogicalSize::new(width, ISLAND_H));
            {
                let mut m = motion.lock().unwrap();
                m.hidden = false;
            }
            // 解除穿透立即生效：看护线程 90ms 粒度太慢，滑入途中收不到悬停
            let _ = win.set_ignore_cursor_events(false);
            // 贴边停靠：滑回停靠位（Out：减速进场）；自由位原地亮出，无需滑动
            if edge != "none" {
                let store = app.state::<Arc<Store>>();
                if let Some(docked) = saved_pos(&store) {
                    slide_to(
                        &win,
                        docked,
                        mon.scale_factor(),
                        &motion,
                        Slide::Out(SLIDE_SHOW_MS),
                    );
                }
            }
            let _ = app.emit_to(
                ISLAND,
                "island-dock",
                serde_json::json!({ "edge": edge, "hidden": false }),
            );
            let _ = win.show();
            if summon {
                // 广播"托盘召唤"给前端计时（3s 无操作自动收回，鼠标移入即取消）
                let _ = app.emit_to(ISLAND, "island-summon", ());
            }
            log::debug!("[岛] 转换 Pill：edge={edge} summon={summon}");
        }
    }
}

/// 拖放后的贴靠评估：吸附到最近的屏幕边，按设置决定是否滑出隐藏；自由位置则原样记忆。
/// 全程使用逻辑坐标（物理事件坐标按显示器缩放换算），与窗口逻辑尺寸/前端 CSS 一致
fn apply_snap(
    app: &tauri::AppHandle,
    store: &Store,
    motion: &Arc<Mutex<IslandMotion>>,
    pos: (i32, i32),
) {
    let Some(win) = app.get_webview_window(ISLAND) else {
        return;
    };
    let Ok(Some(mon)) = win.current_monitor() else {
        return;
    };
    let scale = mon.scale_factor();
    let ml = monitor_logical(&mon);
    let pos_l = phys_to_logical(pos, scale);
    let width = island_width(ml.2);
    let size = (width, ISLAND_H);

    let edge = detect_edge(pos_l, ml, size, SNAP_THRESHOLD);
    let (docked, hide) = match edge {
        "top" => (
            (
                pos_l.0.clamp(ml.0, ml.0 + ml.2 - size.0),
                ml.1,
            ),
            autohide_enabled(store),
        ),
        "left" => (
            (ml.0, pos_l.1.clamp(ml.1, ml.1 + ml.3 - size.1)),
            autohide_enabled(store),
        ),
        "right" => (
            (
                ml.0 + ml.2 - size.0,
                pos_l.1.clamp(ml.1, ml.1 + ml.3 - size.1),
            ),
            autohide_enabled(store),
        ),
        _ => (pos_l, false),
    };
    {
        let mut m = motion.lock().unwrap();
        m.edge = edge.to_string();
    }
    store.set_setting("island_pos", &format!("{},{}", docked.0, docked.1));
    store.set_setting("island_edge", edge);
    // 显隐交由统一状态机（edge/坐标已持久化，转换函数据此取落点）；
    // 状态迁移留痕（排障主线索："岛不贴边/位置不对/消失"靠它重建时间线）
    log::debug!("[岛] 拖放吸附：edge={} hide={} 落点 {:?}", edge, hide, docked);
    island_transition(
        app,
        if hide {
            IslandTarget::Peek { jump: false }
        } else {
            IslandTarget::Pill { summon: false }
        },
    );
}

/// 左键是否按住（拖拽进行中不评估贴靠，防止拖到半路被吸附走）
fn lbutton_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    // SAFETY:GetAsyncKeyState 仅查询系统全局按键状态，无指针/生命周期风险；
    // 返回值短时置位语义与本用途（按住检测）兼容
    unsafe { GetAsyncKeyState(VK_LBUTTON.0.into()) as u16 & 0x8000 != 0 }
}

/// 光标是否落在顶部隐藏态标签矩形内（窗口内水平居中、底部 PEEK_TOP_H 逻辑像素）。
/// 看护线程 90ms 一次调停穿透开关的依据；窗口不缩窄（缩窗会触发 WebView2 重布局
/// 拉伸伪影，且宽窄跳变与 Moved 防误判协议互相干扰），改由穿透实现"窄标签"语义：
/// 穿透时两侧透明区域把点击还给下层窗口，光标进标签才解除（悬停区=可见区）
fn cursor_on_top_tab(win: &tauri::WebviewWindow) -> bool {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let Ok(Some(mon)) = win.current_monitor() else {
        return false;
    };
    let Ok(p) = win.outer_position() else {
        return false;
    };
    let s = mon.scale_factor();
    let ml_w = monitor_logical(&mon).2;
    let w = island_width(ml_w) as f64;
    let pw = peek_top_width(ml_w) as f64;
    // 标签矩形（物理像素）：水平居中（窗口恒全宽，标签中心与胶囊中心天然对齐）、贴窗口底
    let x0 = p.x as f64 + (w - pw) / 2.0 * s;
    let y0 = p.y as f64 + (ISLAND_H - PEEK_TOP_H) as f64 * s;
    let y1 = p.y as f64 + ISLAND_H as f64 * s;
    let mut pt = POINT::default();
    // SAFETY:GetCursorPos 仅查询系统全局光标坐标，无指针/生命周期风险
    if unsafe { GetCursorPos(&mut pt) }.is_err() {
        return false;
    }
    let (px, py) = (pt.x as f64, pt.y as f64);
    px >= x0 && px < x0 + pw * s && py >= y0 && py < y1
}

/// 切换岛窗口鼠标穿透。带状态缓存：期望与缓存一致时不发系统调用，
/// 避免 90ms 看护节拍下重复 SetWindowLong；island_transition 显示路径绕过缓存
/// 直接解除后，此处下一轮比对会自动收敛（缓存与实际短暂失配无害）
fn set_click_through(win: &tauri::WebviewWindow, on: bool, last: &mut bool) {
    if on == *last {
        return;
    }
    *last = on;
    let _ = win.set_ignore_cursor_events(on);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // （2026-09-17 审查 3.1）tauri-plugin-opener 全项目零引用，已随依赖移除
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        // 保存对话框（报表 CSV 导出让用户自选目录；权限仅 dialog:allow-save）
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            focus_session,
            get_settings,
            set_setting,
            hooks_status,
            install_hooks,
            uninstall_hooks,
            autostart_get,
            autostart_set,
            open_repository,
            report_snapshot,
            session_options,
            session_page,
            session_detail,
            export_sessions_csv,
            open_file_location,
            show_report_window,
            show_sessions_window,
            island_peek,
            island_refresh,
            island_dock_state,
            island_drag_start,
            island_metrics,
            tray_menu_action,
            log_frontend
        ])
        .setup(|app| {
            // 自库：%APPDATA%\com.agenttrackerisland.app\agenttrackerisland.db
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            // 日志与 panic 钩子必须先于数据库打开初始化（2026-09-17 二次审查）：
            // 数据库损坏/迁移失败导致"应用起不来"是最严重的故障，恰恰最需要留痕——
            // 此前 init 排在其后，启动失败完全无痕
            logging::init(&dir.join("logs"));
            logging::install_panic_hook();
            let store = match Store::open(&dir.join("agenttrackerisland.db")) {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    log::error!("数据库打开/迁移失败，应用无法启动：{e:#}");
                    return Err(e.into());
                }
            };
            app.manage(store.clone());
            // 恢复开发者模式（设置页开关：Debug 级细节日志，即时生效）
            if store.get_setting("dev_mode").as_deref() == Some("1") {
                logging::set_verbose(true);
            }
            // 启动配置快照：排障时日志开头即见"程序当时认为的配置"，
            // 与 [设置] 变更日志拼出完整配置时间线（敏感键不落日志）
            log::debug!(
                "[设置] 启动加载：autohide={}，hover={}，cleanup_days={}，agents={}",
                store.get_setting("island_autohide").map(|v| v != "0").unwrap_or(true),
                store.get_setting("hover_expand").map(|v| v != "0").unwrap_or(true),
                store.get_setting("cleanup_days").unwrap_or_else(|| "365".into()),
                store.get_setting("agents_enabled").unwrap_or_else(|| "全部启用".into()),
            );

            let win = app
                .get_webview_window(ISLAND)
                .ok_or_else(|| anyhow::anyhow!("island 窗口未在配置中定义"))?;

            // 毛玻璃：❌不使用 window-vibrancy——Acrylic 是窗口级效果，会把整个
            // 矩形窗口染成磨砂灰，破坏胶囊形态；岛的正确做法=窗口全透明+CSS 自绘
            // 背景（见 App.css）。依赖已移除（M1-5 全宽形态划出，保留理由失效）

            // 贴边/拖拽运动状态（setup 内创建，commands 与看护线程经 manage 共享）；
            // 必须先于 position_island 创建，启动定位要经它防误判
            let motion = Arc::new(Mutex::new(IslandMotion {
                edge: store
                    .get_setting("island_edge")
                    .unwrap_or_else(|| "none".into()),
                ..Default::default()
            }));
            app.manage(motion.clone());

            // 定位：记忆坐标优先，否则顶部居中
            position_island(&win, store.as_ref(), &motion);

            // 窗口移动事件：程序化滑动的落点被消费忽略；用户拖拽则记录待评估位置
            {
                let motion2 = motion.clone();
                win.on_window_event(move |ev| {
                    let tauri::WindowEvent::Moved(pos) = ev else {
                        return;
                    };
                    let mut m = motion2.lock().unwrap();
                    if let Some((since, target)) = m.animating {
                        // 动画产生的移动：仅当到达落点时消费并结算动画
                        if (pos.x, pos.y) == target {
                            m.animating = None;
                            m.programmed = None;
                            drop(m);
                            SLIDE_GEN.fetch_add(1, Ordering::Relaxed);
                        } else if since.elapsed().as_millis() > ANIM_TIMEOUT_MS {
                            // 落点事件意外丢失：超时自复位，防止动画标记永久卡死
                            m.animating = None;
                            m.programmed = None;
                        }
                        return;
                    }
                    m.programmed = None;
                    // 用户拖拽：打断在播动画，交给看护线程防抖评估
                    drop(m);
                    SLIDE_GEN.fetch_add(1, Ordering::Relaxed);
                    motion2.lock().unwrap().pending =
                        Some((std::time::Instant::now(), pos.x, pos.y));
                });
            }

            // 贴靠看护线程：位置静默且左键释放（拖放完成）后评估吸附/隐藏；
            // 兼管顶部隐藏态的鼠标穿透调停（与防抖同频 90ms，开销为微秒级坐标查询）
            {
                let motion2 = motion.clone();
                let app2 = app.handle().clone();
                let store2 = store.clone();
                let win2 = win.clone();
                std::thread::spawn(move || {
                    // 穿透开关缓存：期望与缓存一致时不发系统调用（见 set_click_through）
                    let mut last_ct = false;
                    loop {
                        std::thread::sleep(Duration::from_millis(90));
                        let fire = {
                            let mut m = motion2.lock().unwrap();
                            match m.pending.take() {
                                Some((at, x, y))
                                    if at.elapsed().as_millis() >= DRAG_QUIET_MS
                                        && !lbutton_down() =>
                                {
                                    Some((x, y))
                                }
                                Some(other) => {
                                    m.pending = Some(other);
                                    None
                                }
                                None => None,
                            }
                        };
                        if let Some((x, y)) = fire {
                            apply_snap(&app2, &store2, &motion2, (x, y));
                        }
                        // 顶部隐藏态：光标进标签矩形才解除穿透（悬停区=可见区），
                        // 其余状态一律确保解除，防止拖离顶部后岛整体不可点
                        let ct_want = {
                            let m = motion2.lock().unwrap();
                            m.edge == "top" && m.hidden
                        };
                        let ct_on = if ct_want {
                            cursor_on_top_tab(&win2)
                        } else {
                            false
                        };
                        set_click_through(&win2, ct_on, &mut last_ct);
                    }
                });
            }

            // 启动恢复：上次贴边 + 自动隐藏开启 → 瞬移滑出（仅露边）；其余按记忆坐标可见。
            // 走统一转换函数：此时 webview 未挂载，island-dock 事件无接收者，
            // 前端挂载后经 island_dock_state 拉取补齐首帧状态（原有时序）
            {
                let edge = motion.lock().unwrap().edge.clone();
                if edge != "none" && autohide_enabled(store.as_ref()) {
                    island_transition(app.handle(), IslandTarget::Peek { jump: true });
                    log::debug!("[岛] 启动恢复贴边隐藏：edge={edge}");
                }
            }

            // 设置/会话/报表/关于/托盘菜单窗口：关闭即隐藏（而非销毁），保证托盘可反复唤起
            for label in ["settings", "sessions", "report", "about", "tray-menu"] {
                if let Some(w) = app.get_webview_window(label) {
                    let w2 = w.clone();
                    w.on_window_event(move |ev| {
                        if let tauri::WindowEvent::CloseRequested { api, .. } = ev {
                            api.prevent_close();
                            let _ = w2.hide();
                        }
                    });
                }
            }

            // 托盘菜单失焦即收（B1 焦点策略）：点击外部/切走焦点 → 隐藏。
            // hide 是幂等窗口操作，无需防抖；Esc 与菜单项选中由前端自隐补充
            if let Some(tm) = app.get_webview_window("tray-menu") {
                let tm2 = tm.clone();
                tm.on_window_event(move |ev| {
                    if let tauri::WindowEvent::Focused(false) = ev {
                        let _ = tm2.hide();
                    }
                });
            }

            build_tray(app)?;

            // 数据清理：按设置周期启动时执行一次（0=永不；未设置默认保留 1 年，
            // 审查 3.8：status_events 每工具调用一条，永不清理会让库无限膨胀）
            let cleanup_days = store
                .get_setting("cleanup_days")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(365);
            if cleanup_days > 0 {
                let cutoff = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64 - cleanup_days * 86_400_000)
                    .unwrap_or(0);
                let removed = store.cleanup_older_than(cutoff);
                if removed > 0 {
                    log::info!("启动清理：按保留 {cleanup_days} 天删除 {removed} 条过期数据");
                }
            }

            spawn_aggregator(app.handle().clone(), store);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// 岛定位：有记忆坐标用之（逻辑坐标，夹取进显示器防丢失）；否则按主显示器顶部居中。
/// 必须经 motion 通道移动（slide_to 瞬时档）：否则启动定位产生的 Moved 事件
/// 会被看护线程误判为"拖放"，把距顶边仅 6px 的岛自动吸附隐藏
fn position_island(
    win: &tauri::WebviewWindow,
    store: &Store,
    motion: &Arc<Mutex<IslandMotion>>,
) {
    let Ok(Some(mon)) = win.current_monitor() else {
        return;
    };
    let ml = monitor_logical(&mon);
    let width = island_width(ml.2);
    let remembered = saved_pos(store);
    let pos = match remembered {
        Some(p) => (
            p.0.clamp(ml.0, ml.0 + ml.2 - width),
            p.1.clamp(ml.1, ml.1 + ml.3 - ISLAND_H),
        ),
        None => (ml.0 + (ml.2 - width) / 2, ml.1 + 6),
    };
    log::debug!(
        "[岛] 启动定位：落点 {:?}（{}）",
        pos,
        if remembered.is_some() { "记忆坐标" } else { "默认居中" }
    );
    slide_to(win, pos, mon.scale_factor(), motion, Slide::Jump);
}

// ===== 托盘菜单（webview 自绘，M1-9） =====
// 原生托盘菜单（muda → Win32 弹出菜单）的行高/间距/字体全部由系统决定，muda 无
// 样式 API（已核对 0.19.3 源码，图标位图亦硬编码 16×16），"更舒展精致"在原生菜单
// 内无解。改为无边框置顶小窗（#tray-menu，tauri.conf.json）自绘：行高/间距/圆角/
// 动效/双主题全部可控，样式只引用岛面板的 CSS 变量体系。菜单动作与岛显隐共用
// 同一批内部函数（单一事实源，见 tray_menu_action）；显隐语义不变（隐藏=彻底
// 消失/显示=临时召唤，玩游戏前一键隐藏玩完召回的场景照旧），见 toggle_island。

/// 托盘左键动作（设置项 tray_left_action）：none=无操作（默认）｜toggle=显隐灵动岛｜
/// menu=打开托盘菜单。右键恒为打开托盘菜单，不进设置（Windows 托盘通用惯例）
fn tray_left_action(store: &Store) -> &'static str {
    match store.get_setting("tray_left_action").as_deref() {
        Some("toggle") => "toggle",
        Some("menu") => "menu",
        _ => "none",
    }
}

/// 托盘菜单落点（物理像素）：从托盘图标矩形向屏幕内侧弹出——任务栏在底/顶时
/// 垂直弹出（菜单水平中心对齐图标中心），在左/右时水平弹出（垂直中心对齐），
/// 最后整体夹取进显示器矩形。任务栏所在边 = 图标中心距哪条屏幕边最近；
/// 平局优先级 底 > 顶 > 左 > 右。独立纯函数便于几何单测
fn tray_menu_pos(
    icon: (i32, i32, i32, i32), // 托盘图标矩形 x y w h（物理）
    menu: (i32, i32),           // 菜单窗口尺寸 w h（物理）
    mon: (i32, i32, i32, i32),  // 显示器矩形 x y w h（物理）
    gap: i32,                   // 菜单与托盘图标的间距
) -> (i32, i32) {
    let (ix, iy, iw, ih) = icon;
    let (mw, mh) = menu;
    let (mx, my, mo_w, mo_h) = mon;
    let (icx, icy) = (ix + iw / 2, iy + ih / 2);
    let (dl, dr) = (icx - mx, mx + mo_w - icx);
    let (dt, db) = (icy - my, my + mo_h - icy);
    let (mut x, mut y) = if db <= dt && db <= dl && db <= dr {
        (icx - mw / 2, iy - gap - mh) // 任务栏在底：菜单向上弹
    } else if dt <= dl && dt <= dr {
        (icx - mw / 2, iy + ih + gap) // 任务栏在顶：向下弹
    } else if dl <= dr {
        (ix + iw + gap, icy - mh / 2) // 任务栏在左：向右弹
    } else {
        (ix - gap - mw, icy - mh / 2) // 任务栏在右：向左弹
    };
    // 夹取进显示器（菜单远小于屏幕，clamp 两侧不会越界成 min>max）
    x = x.clamp(mx, mx + mo_w - mw);
    y = y.clamp(my, my + mo_h - mh);
    (x, y)
}

/// 光标点所在显示器：托盘事件矩形是物理坐标，而菜单窗口隐藏时 current_monitor
/// 不可靠，按"图标中心落在哪块屏"定位所属显示器
fn monitor_at(app: &tauri::AppHandle, pt: (f64, f64)) -> Option<tauri::Monitor> {
    app.available_monitors().ok()?.into_iter().find(|m| {
        let p = m.position();
        let s = m.size();
        let (x1, y1) = (p.x as f64, p.y as f64);
        let (x2, y2) = (x1 + s.width as f64, y1 + s.height as f64);
        pt.0 >= x1 && pt.0 < x2 && pt.1 >= y1 && pt.1 < y2
    })
}

/// 弹出托盘菜单：按托盘事件自带的图标矩形定位（零插件依赖），推送最新岛可见性
/// 后显示并聚焦（B1 焦点策略：用户点击托盘属预期交互；失焦即收，见 setup 的
/// Focused 事件与菜单页 Esc）。已可见时再点右键 = 重新定位并保持
fn show_tray_menu(app: &tauri::AppHandle, rect: tauri::Rect) {
    let Some(win) = app.get_webview_window("tray-menu") else {
        log::debug!("[托盘] 菜单窗口不存在，右键无效果");
        return;
    };
    let Ok(size) = win.outer_size() else {
        return;
    };
    // 事件矩形为物理坐标（Position/Size 枚举的 Physical 分支原样返回）
    let rpos = rect.position.to_physical(1.0);
    let rsize = rect.size.to_physical::<u32>(1.0);
    let center = (
        rpos.x as f64 + rsize.width as f64 / 2.0,
        rpos.y as f64 + rsize.height as f64 / 2.0,
    );
    let Some(mon) = monitor_at(app, center) else {
        return;
    };
    let mp = mon.position();
    let ms = mon.size();
    let pos = tray_menu_pos(
        (rpos.x, rpos.y, rsize.width as i32, rsize.height as i32),
        (size.width as i32, size.height as i32),
        (mp.x, mp.y, ms.width as i32, ms.height as i32),
        8,
    );
    let _ = win.set_position(tauri::PhysicalPosition::new(pos.0, pos.1));
    // 推送最新岛可见性（菜单项文案/徽标用）：弹出时刻的快照比菜单页内存态可靠；
    // 附带的 show 事件同时驱动前端重放入场动画
    let island_visible = app
        .get_webview_window(ISLAND)
        .is_some_and(|w| w.is_visible().unwrap_or(true));
    let _ = app.emit_to(
        "tray-menu",
        "tray-menu-show",
        serde_json::json!({ "island_visible": island_visible }),
    );
    let _ = (win.show(), win.set_focus());
    log::debug!("[托盘] 菜单弹出：落点 {:?}", pos);
}

/// 系统托盘：常驻核心。左键动作由设置项 tray_left_action 决定（默认无操作，
/// 选择权交给用户），右键恒为弹出 webview 自绘托盘菜单；双击不响应（单击/双击
/// 消歧需要延迟等待，单击手感变钝，已与所有者确认放弃双击）。
/// 事件回调转后台线程执行：与旧 on_menu_event 同款主线程减负（内含 SQLite 读）
fn build_tray(app: &tauri::App) -> anyhow::Result<()> {
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    TrayIconBuilder::with_id("at-tray")
        .tooltip("去你的岛 · AgentTrackerIsland")
        .icon(app.default_window_icon().expect("应用图标").clone())
        .on_tray_icon_event(|tray, ev| {
            // 只认"松开"的 Click：按住即弹会让菜单出现在按压期间；DoubleClick/
            // Enter/Move 一律忽略。事件自带图标矩形（rect），定位零插件依赖
            let TrayIconEvent::Click { rect, button, button_state, .. } = ev else {
                return;
            };
            if button_state != MouseButtonState::Up {
                return;
            }
            let app = tray.app_handle().clone();
            std::thread::spawn(move || match button {
                MouseButton::Left => {
                    let store = app.state::<Arc<Store>>();
                    match tray_left_action(&store) {
                        "toggle" => toggle_island(&app),
                        "menu" => show_tray_menu(&app, rect),
                        _ => {} // none：无操作（默认档）
                    }
                }
                MouseButton::Right => show_tray_menu(&app, rect),
                _ => {}
            });
        })
        .build(app)?;
    log::debug!("[托盘] 已注册（左键按设置项分发，右键自绘菜单）");
    Ok(())
}

/// 显示并聚焦辅助窗口（设置/报表/关于：常驻隐藏窗口，只 show 不重建）
fn show_aux_window(app: &tauri::AppHandle, label: &str) {
    if let Some(w) = app.get_webview_window(label) {
        let _ = (w.show(), w.set_focus());
    }
}

/// 托盘切换灵动岛显隐（"隐藏"=彻底消失，"显示"=临时召唤）。
/// 贴边隐藏态的窗口仍在屏外"可见"（is_visible=true）——隐藏是整窗 hide；
/// 显示时贴边停靠滑回停靠位亮相（summon：3s 无操作自动收回，鼠标移入即
/// 取消，见 App.tsx island-summon），自由摆放原地亮出、无自动收起。
/// 编排细节全部在 island_transition（显隐唯一入口）
fn toggle_island(app: &tauri::AppHandle) {
    let Some(w) = app.get_webview_window(ISLAND) else {
        return;
    };
    if w.is_visible().unwrap_or(false) {
        // 任意可见态（胶囊或贴边标签）→ 整窗隐藏；停靠坐标仍记忆，供下次召回
        island_transition(app, IslandTarget::Gone);
        return;
    }
    // 隐藏中 → 召回
    let edge = app
        .state::<Arc<Mutex<IslandMotion>>>()
        .lock()
        .unwrap()
        .edge
        .clone();
    island_transition(
        app,
        if edge != "none" {
            IslandTarget::Pill { summon: true }
        } else {
            IslandTarget::Pill { summon: false }
        },
    );
}

/// 托盘菜单动作分发（webview 菜单项 → Rust）：白名单枚举，全部复用托盘旧有
/// 动作逻辑——岛显隐 toggle_island、开窗 show_aux_window、退出 app.exit。
/// toggle 转后台线程：与旧菜单回调同款主线程减负（内部可能触发滑动动画与 SQLite 读）
#[tauri::command]
fn tray_menu_action(app: tauri::AppHandle, action: String) -> Result<(), String> {
    match action.as_str() {
        "toggle" => {
            std::thread::spawn(move || toggle_island(&app));
            Ok(())
        }
        "report" => {
            show_aux_window(&app, "report");
            Ok(())
        }
        "sessions" => {
            show_aux_window(&app, "sessions");
            Ok(())
        }
        "settings" => {
            show_aux_window(&app, "settings");
            Ok(())
        }
        "about" => {
            show_aux_window(&app, "about");
            Ok(())
        }
        "quit" => {
            app.exit(0);
            Ok(())
        }
        _ => Err(format!("未知托盘菜单动作：{action}")),
    }
}

/// 相邻两轮 tick 的最小间隔（毫秒）：快轮风暴下 tick 也不得快于该值；
/// 护栏丢弃的唤醒不补偿，至多一个调度周期后自然到期（04-EXPANSION M2-1）
const MIN_TICK_GAP_MS: u64 = 250;
/// 快轮采样周期（毫秒）：恒定 1s 逐信号 stat/浅枚举，成本微秒级无需预算
const HOT_POLL_MS: u64 = 1000;

/// 后台聚合调度（M2-1 调度器骨架，替代原固定 10s sleep；04-EXPANSION §2.4）：
///   主环：recv_timeout 唤醒——快轮命中立即 tick，否则按自适应间隔
///   （有会话工作中/等待 1s；有会话但全空闲 5s；无会话 10s）；
///   快轮线程：1s 采样各适配器 HotSignal，采样值变化才投递唤醒
///   （channel 容量 1，try_send 满即丢，天然合并风暴）；
///   广播去重：快照内容签名（generated_at 除外）未变化则跳过 emit，
///   1s 节拍下前端零无效重渲染。
/// tick 全程 catch_unwind（审查 1.1）：单轮 panic 不允许杀死线程造成岛永久
/// 静默冻结——panic 已由全局钩子落盘，线程降级续跑，快照带 degraded 标志
fn spawn_aggregator(app: tauri::AppHandle, store: Arc<Store>) {
    std::thread::spawn(move || {
        let mut agg = Aggregator::new(store);
        // 快轮信号在 agg 被移入主环前取出（信号是自足的声明，不依赖 agg 存活）
        let signals = agg.hot_signals();
        // 容量 1 的同步通道：try_send 满即丢，天然合并唤醒风暴
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        {
            // 快轮线程：恒定 1s 采样；last 初始全 None，「从无到有」首现即唤醒
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut last: Vec<Option<crate::collector::engine::SignalValue>> =
                    vec![None; signals.len()];
                loop {
                    std::thread::sleep(Duration::from_millis(HOT_POLL_MS));
                    for (i, s) in signals.iter().enumerate() {
                        let v = s.sample();
                        if v != last[i] {
                            let _ = tx.try_send(());
                        }
                        last[i] = v;
                    }
                }
            });
        }
        let mut last_sig: Option<u64> = None;
        let mut last_tick = std::time::Instant::now();
        // 初始按"无会话"档进入：首轮立即 tick，随后由快照驱动档位切换
        let (mut any_active, mut has_sessions) = (false, false);
        loop {
            let interval = if any_active {
                Duration::from_millis(1000)
            } else if has_sessions {
                Duration::from_millis(5000)
            } else {
                Duration::from_millis(10000)
            };
            let wake_at = last_tick + interval;
            let now = std::time::Instant::now();
            if wake_at > now {
                // 快轮命中提前唤醒 / 到期自然唤醒，二者先到先算
                let _ = rx.recv_timeout(wake_at - now);
            }
            if last_tick.elapsed() < Duration::from_millis(MIN_TICK_GAP_MS) {
                continue;
            }
            last_tick = std::time::Instant::now();
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| agg.tick()));
            match result {
                Ok(mut snap) => {
                    // 档位驱动：有会话工作中/等待 → 1s 快档；其余按有无会话降档
                    any_active = snap
                        .sessions
                        .iter()
                        .any(|s| matches!(s.state, crate::state::SessionState::Working | crate::state::SessionState::Waiting));
                    has_sessions = !snap.sessions.is_empty();
                    // 广播签名去重：内容不变不 emit（generated_at 不参与签名）
                    snap.generated_at = 0;
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    serde_json::to_string(&snap).unwrap_or_default().hash(&mut hasher);
                    let sig = hasher.finish();
                    if last_sig != Some(sig) {
                        last_sig = Some(sig);
                        snap.generated_at = now_ms();
                        log::debug!(
                            "[聚合] 广播快照（签名更新，下轮档位：{}ms，会话 {}）",
                            if any_active { 1000 } else if has_sessions { 5000 } else { 10000 },
                            snap.sessions.len()
                        );
                        // 前端未监听时 emit 也只是无接收者，不报错
                        let _ = app.emit("island-snapshot", &snap);
                    }
                }
                Err(payload) => {
                    log::error!(
                        "聚合 tick panic（本轮无快照，线程续跑）：{}",
                        logging::panic_payload_str(payload)
                    );
                }
            }
        }
    });
}

/// 当前 Unix 毫秒（广播时间戳回填用）
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 贴边几何判定：阈值内吸附，优先级 上 > 左 > 右
    #[test]
    fn test_detect_edge() {
        let mon = (0, 0, 1920, 1080); // 显示器矩形
        let size = (480, 48);
        // 屏幕中央：自由位置
        assert_eq!(detect_edge((720, 500), mon, size, SNAP_THRESHOLD), "none");
        // 顶部各处（含角落，顶部优先）
        assert_eq!(detect_edge((960, 10), mon, size, SNAP_THRESHOLD), "top");
        assert_eq!(detect_edge((5, 5), mon, size, SNAP_THRESHOLD), "top");
        // 左侧（不在顶部阈值内）
        assert_eq!(detect_edge((3, 500), mon, size, SNAP_THRESHOLD), "left");
        // 右侧：窗口右缘距屏幕右缘 20px <= 24
        assert_eq!(detect_edge((1920 - 460, 500), mon, size, SNAP_THRESHOLD), "right");
        // 恰好等于阈值：仍吸附（<=）
        assert_eq!(detect_edge((24, 500), mon, size, SNAP_THRESHOLD), "left");
        // 超出阈值一个像素：不吸附
        assert_eq!(detect_edge((25, 500), mon, size, SNAP_THRESHOLD), "none");
        // 顶部超出阈值：落空到左侧判断
        assert_eq!(detect_edge((960, 25), mon, size, SNAP_THRESHOLD), "none");
    }

    /// 隐藏位计算：三种贴靠边各露 PEEK_PX 在屏内
    #[test]
    fn test_hidden_pos() {
        let mon_pos = (0, 0);
        let mon_w = 1920;
        let width = island_width(mon_w); // 480
        // 顶部：上滑，底部露出 PEEK_TOP_H（独立信息条）
        assert_eq!(hidden_pos(mon_pos, mon_w, "top", (720, 0), width), (720, -(ISLAND_H - PEEK_TOP_H)));
        // 左侧：左滑，右侧露出 PEEK_SIDE_W（半圆标签）
        assert_eq!(hidden_pos(mon_pos, mon_w, "left", (0, 300), width), (-(width - PEEK_SIDE_W), 300));
        // 右侧：右滑，左缘留在 1920-PEEK_SIDE_W
        assert_eq!(hidden_pos(mon_pos, mon_w, "right", (1440, 300), width), (1920 - PEEK_SIDE_W, 300));
    }

    /// 自适应宽度：按屏幕逻辑宽 30% 夹取 [380， 800]
    #[test]
    fn test_island_width() {
        assert_eq!(island_width(1280), 384);
        assert_eq!(island_width(1366), 410); // 所有者笔记本：+1/5 手感校准
        assert_eq!(island_width(1440), 432);
        assert_eq!(island_width(1920), 576);
        assert_eq!(island_width(2560), 768);
        assert_eq!(island_width(3840), 800); // 大屏封顶
    }

    /// 托盘菜单落点：任务栏四方位向屏内弹出，越界夹取进显示器
    #[test]
    fn test_tray_menu_pos() {
        let mon = (0, 0, 1920, 1080);
        let menu = (280, 300);
        let gap = 8;
        // 底部任务栏（图标居中）：向上弹，水平中心对齐图标中心
        assert_eq!(
            tray_menu_pos((960, 1040, 32, 32), menu, mon, gap),
            (976 - 140, 1040 - gap - 300)
        );
        // 顶部任务栏：向下弹
        assert_eq!(
            tray_menu_pos((960, 8, 32, 32), menu, mon, gap),
            (976 - 140, 8 + 32 + gap)
        );
        // 左侧任务栏：向右弹，垂直中心对齐
        assert_eq!(
            tray_menu_pos((8, 520, 32, 32), menu, mon, gap),
            (8 + 32 + gap, 536 - 150)
        );
        // 右侧任务栏：向左弹
        assert_eq!(
            tray_menu_pos((1880, 520, 32, 32), menu, mon, gap),
            (1880 - gap - 280, 536 - 150)
        );
        // 底任务栏右端图标：水平越界被夹取进屏幕右缘（db=dr 平局时底优先）
        let (x, y) = tray_menu_pos((1880, 1040, 32, 32), menu, mon, gap);
        assert_eq!(x, 1920 - 280);
        assert_eq!(y, 1040 - gap - 300);
        // 矮屏兜底：垂直放不下时夹取到屏幕顶
        let (_, y) = tray_menu_pos((960, 260, 32, 32), menu, (0, 0, 1920, 300), gap);
        assert_eq!(y, 0);
    }
}
