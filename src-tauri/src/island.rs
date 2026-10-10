//! 岛运动学域（2026-10-03 四轮审查拆分自 lib.rs，纯移动零逻辑变更）：
//! 窗口常量与自适应公式 / IslandMotion 状态机 / 滑动动画 / 贴边几何与看护 /
//! 鼠标穿透调停 / 启动定位，及对应的贴边 commands。
//! run()（lib.rs）与本模块经 Arc<Mutex<IslandMotion>> 共享运动状态。
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tauri::{Emitter, Manager};
use crate::store::Store;

pub(crate) const ISLAND: &str = "island";

// ===== 贴边自动隐藏（追加需求：自由拖拽 + 贴边隐藏，默认开启） =====

/// 岛收缩态固定高度（逻辑像素）；宽度按屏幕自适应，见 island_width()
pub(crate) const ISLAND_H: i32 = 48;
/// 岛展开面板高度（逻辑像素）
pub(crate) const ISLAND_EXPANDED_H: i32 = 520;
/// 自适应宽度：显示器逻辑宽 × 比例，夹取 [MIN， MAX]（小屏保底、大屏封顶）。
/// 比例取 30%：介于三分律（1/3）与黄金分割小段（0.382）之间、主流悬浮组件
/// 25%~35% 区间的中值（NN/g、Figma 设计参考；所有者笔记本实测 +1/5 手感吻合）
pub(crate) const ISLAND_W_RATIO: f64 = 0.30;
pub(crate) const ISLAND_W_MIN: i32 = 380;
pub(crate) const ISLAND_W_MAX: i32 = 800;
/// 顶部贴边隐藏的露出高度（≈胶囊 48 的 1/5 略加余量：胶囊底部的独立信息条）
pub(crate) const PEEK_TOP_H: i32 = 14;
/// 左右贴边隐藏的伸出宽度（半圆 D 形标签）
pub(crate) const PEEK_SIDE_W: i32 = 20;
/// 贴靠判定阈值（逻辑像素）：拖放位置距屏幕边小于该值即吸附到该边
pub(crate) const SNAP_THRESHOLD: i32 = 24;
/// 拖拽防抖：Moved 事件静默该时长且左键已释放，才认定"拖放完成"并评估贴靠
pub(crate) const DRAG_QUIET_MS: u128 = 180;

/// 岛自适应宽度：显示器逻辑宽 × 30%，夹取 [380， 800]。
/// 1366→410 / 1440→432 / 1920→576 / 2560→768 / 3840→800；Rust 贴边几何与前端渲染共用
pub(crate) fn island_width(mon_logical_w: i32) -> i32 {
    ((mon_logical_w as f64 * ISLAND_W_RATIO).round() as i32)
        .clamp(ISLAND_W_MIN, ISLAND_W_MAX)
}

/// 顶部贴边隐藏态宽度：胶囊公式的常数全部减半（比例/上下限各取 1/2），
/// 与胶囊恒保持 2:1——任意屏幕上"窄标签 → 全宽胶囊"的生长比例一致。
/// 1366→205 / 1920→288 / 2560→384 / ≥2667→400；经 island_metrics 下发前端渲染
pub(crate) fn peek_top_width(mon_logical_w: i32) -> i32 {
    ((mon_logical_w as f64 * ISLAND_W_RATIO * 0.5).round() as i32)
        .clamp(ISLAND_W_MIN / 2, ISLAND_W_MAX / 2)
}

/// 岛的运动/贴边状态（内存态，setup 时创建并全局共享；位置与开关持久化在 app_settings）
#[derive(Default)]
pub(crate) struct IslandMotion {
    /// 拖拽中的待评估位置（事件时间戳 + 坐标），看护线程防抖后消费
    pub(crate) pending: Option<(std::time::Instant, i32, i32)>,
    /// 程序化滑动（吸附/隐藏/显示）的目标落点：用于识别并消费动画自己产生的 Moved 事件
    pub(crate) programmed: Option<(i32, i32)>,
    /// 滑动动画进行中：开始时刻 + 落点。Moved 事件消费落点即结束；
    /// 落点事件意外丢失时按超时自复位，防"动画标记永久卡死"（审查 3.7:
    /// 由独立的兜底线程改为时间戳判定，少一个短命线程）
    pub(crate) animating: Option<(std::time::Instant, (i32, i32))>,
    /// 当前贴靠边："none" | "top" | "left" | "right"
    pub(crate) edge: String,
    /// 是否处于滑出隐藏态
    pub(crate) hidden: bool,
    /// 启动原位展示待回位（2026-10-08 改版：启动定位把胶囊放在记忆坐标原位
    /// 展示「启动中…」后置位；前端收到首个数据快照调用 island_boot_settled 时
    /// 消费——贴边按自动隐藏设置转 Peek/Pill，自由位原地不动）；用户拖放
    /// （apply_snap）即清除，防止与用户抢窗口
    pub(crate) boot_pending: bool,
}

/// 动画标记超时：超过该时长仍未等到落点 Moved 事件则自复位（正常动画约 160~240ms）
pub(crate) const ANIM_TIMEOUT_MS: u128 = 400;

/// 滑动动画代数：每次新滑动/用户拖拽都递增，使旧动画线程自行退出
pub(crate) static SLIDE_GEN: AtomicU64 = AtomicU64::new(0);

// ===== 贴边自动隐藏 commands（追加需求） =====

/// 岛自适应尺寸（前端挂载时获取，Rust 贴边几何与前端渲染共用同一公式）
#[derive(serde::Serialize)]
pub(crate) struct IslandMetrics {
    width: i32,
    collapsed_h: i32,
    expanded_h: i32,
    /// 顶部贴边隐藏态宽度（peek_top_width：胶囊常数减半，恒 2:1）
    peek_top_w: i32,
    /// 工作区底缘（逻辑 y，五轮审查批次三）：前端展开封顶用——岛窗口左上角
    /// 锚定、面板向下生长，岛在屏幕下半部时须按「底缘 − 岛 y」限制展开高度，
    /// 否则面板伸出屏幕外（内容出屏且内部滚动也滚不到）
    work_bottom: i32,
}

/// 查询岛自适应尺寸（基于岛窗口当前所在显示器的逻辑宽度）
#[tauri::command]
pub(crate) fn island_metrics(win: tauri::WebviewWindow) -> Option<IslandMetrics> {
    let mon = win.current_monitor().ok().flatten()?;
    let ml = monitor_logical(&mon);
    let wa = mon.work_area();
    let s = mon.scale_factor();
    let work_bottom = (((wa.position.y + wa.size.height as i32) as f64) / s).round() as i32;
    Some(IslandMetrics {
        width: island_width(ml.2),
        collapsed_h: ISLAND_H,
        expanded_h: ISLAND_EXPANDED_H,
        peek_top_w: peek_top_width(ml.2),
        work_bottom,
    })
}

/// 鼠标移入贴边岛 → 滑入显示（show=true）/移出 → 滑出隐藏（show=false）。
/// 未贴边（edge=none）或关闭自动隐藏时为无害 no-op
#[tauri::command]
pub(crate) fn island_peek(
    show: bool,
    app: tauri::AppHandle,
    store: tauri::State<'_, Arc<Store>>,
    motion: tauri::State<'_, Arc<Mutex<IslandMotion>>>,
) {
    let (edge, hidden) = {
        let m = motion.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
pub(crate) fn island_refresh(
    app: tauri::AppHandle,
    store: tauri::State<'_, Arc<Store>>,
    motion: tauri::State<'_, Arc<Mutex<IslandMotion>>>,
) {
    let (edge, hidden) = {
        let m = motion.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
pub(crate) fn island_dock_state(motion: tauri::State<'_, Arc<Mutex<IslandMotion>>>) -> serde_json::Value {
    let m = motion.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    serde_json::json!({ "edge": m.edge, "hidden": m.hidden })
}

/// 启动完成回调（2026-10-08 启动原位展示改版）：前端收到首个数据快照时调用。
/// 启动胶囊本已在记忆坐标原位，此处按上次退出状态收敛——贴边自动隐藏 →
/// 从停靠位动画滑出隐藏（Peek）；贴边不隐藏 → 滑回停靠位（Pill，已在位则为
/// 等位 no-op）；自由位 → 滑回记忆坐标（已在位则为等位 no-op）。
/// 仅启动态生效：boot_pending 由 position_island 置位、用户拖放（apply_snap）
/// 即清除，运行期重复调用为无害 no-op。锁纪律：island_transition/slide_to
/// 内部会 lock motion，标记消费必须在独立锁块先行（见 island_transition ⚠）
#[tauri::command]
pub(crate) fn island_boot_settled(
    app: tauri::AppHandle,
    motion: tauri::State<'_, Arc<Mutex<IslandMotion>>>,
) {
    {
        let mut m = motion.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !m.boot_pending {
            return;
        }
        m.boot_pending = false;
    }
    let edge = motion.lock().unwrap_or_else(std::sync::PoisonError::into_inner).edge.clone();
    let store = app.state::<Arc<Store>>();
    if edge != "none" {
        // 贴边用户：回到贴边工作态（统一状态机编排，动画过渡非瞬移）
        let target = if autohide_enabled(&store) {
            IslandTarget::Peek { jump: false }
        } else {
            IslandTarget::Pill { summon: false }
        };
        island_transition(&app, target);
        return;
    }
    // 自由位：滑回记忆坐标（夹取进显示器，防换屏/改分辨率后丢到屏幕外）
    let Some(win) = app.get_webview_window(ISLAND) else {
        return;
    };
    let Ok(Some(mon)) = win.current_monitor() else {
        return;
    };
    let Some(docked) = saved_pos(&store) else {
        return; // 记忆坐标缺失（异常）：留在居中位，无害降级
    };
    let ml = monitor_logical(&mon);
    let width = island_width(ml.2);
    let docked = clamp_docked(docked, ml, width);
    log::debug!("[岛] 启动完成：居中位滑回记忆位 {docked:?}");
    slide_to(
        &win,
        docked,
        mon.scale_factor(),
        &motion,
        Slide::Out(SLIDE_SHOW_MS),
    );
}

/// 用户按下岛（拖拽开始）：取消在播滑动动画与待评估位置，
/// 避免拖拽循环和滑动动画互相抢窗口（程序化 set_position 会打断系统拖拽）
#[tauri::command]
pub(crate) fn island_drag_start(motion: tauri::State<'_, Arc<Mutex<IslandMotion>>>) {
    SLIDE_GEN.fetch_add(1, Ordering::Relaxed);
    let mut m = motion.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    m.animating = None;
    m.programmed = None;
    m.pending = None;
    // 用户接管（五轮审查）：清启动回位标记——拖拽进行中首个快照到达时，
    // boot_settled 的程序化滑动会与系统拖拽抢窗口（本函数防的正是程序化
    // set_position 打断拖拽，启动路径不该绕过守卫；与 apply_snap 语义对齐）
    m.boot_pending = false;
}

/// 贴边自动隐藏开关（设置项 island_autohide，缺省=开）
pub(crate) fn autohide_enabled(store: &Store) -> bool {
    store
        .get_setting("island_autohide")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// 解析 island_pos 设置（"x,y" 逻辑坐标；写入方 apply_snap 持久化的是逻辑坐标）
pub(crate) fn saved_pos(store: &Store) -> Option<(i32, i32)> {
    store.get_setting("island_pos").and_then(|s| {
        s.split_once(',')
            .and_then(|(a, b)| match (a.trim().parse::<i32>(), b.trim().parse::<i32>()) {
                (Ok(x), Ok(y)) => Some((x, y)),
                _ => None,
            })
    })
}

/// 物理坐标 → 逻辑坐标（按显示器缩放换算）
pub(crate) fn phys_to_logical(v: (i32, i32), scale: f64) -> (i32, i32) {
    (
        (v.0 as f64 / scale).round() as i32,
        (v.1 as f64 / scale).round() as i32,
    )
}

/// 逻辑坐标 → 物理坐标
pub(crate) fn logical_to_phys(v: (i32, i32), scale: f64) -> (i32, i32) {
    (
        (v.0 as f64 * scale).round() as i32,
        (v.1 as f64 * scale).round() as i32,
    )
}

/// 显示器矩形（逻辑坐标）：x， y， w， h
pub(crate) fn monitor_logical(mon: &tauri::Monitor) -> (i32, i32, i32, i32) {
    let s = mon.scale_factor();
    let mp = mon.position();
    (
        (mp.x as f64 / s).round() as i32,
        (mp.y as f64 / s).round() as i32,
        (mon.size().width as f64 / s).round() as i32,
        (mon.size().height as f64 / s).round() as i32,
    )
}

/// 区间 clamp 的窄屏安全版（五轮审查）：i32::clamp 在 min>max 时 panic——
/// 逻辑宽小于岛宽 380（如 800px 物理屏＋250% 缩放 → 逻辑 320）时
/// `ml.0 + ml.2 - width` 会小于 `ml.0`；退化为贴 lo（左/上缘），宁可
/// 另一侧出屏也不崩（apply_snap 路径有看护线程 catch_unwind 兜底，但
/// clamp_docked 还被同步命令 island_boot_settled 消费，panic ＝ 命令无响应）
fn safe_clamp(v: i32, lo: i32, hi: i32) -> i32 {
    v.clamp(lo, hi.max(lo))
}

/// 记忆停靠位夹取进当前显示器（2026-10-03 审查修复抽取共用）：换屏/改分辨率
/// 后旧坐标可能在屏外——不夹取则「滑向屏外」表现为召回无反应。启动定位与
/// 运行期 Pill 召回两处同用，防一边修一边漏（此前启动路径有 clamp、运行期没有）
pub(crate) fn clamp_docked(docked: (i32, i32), ml: (i32, i32, i32, i32), width: i32) -> (i32, i32) {
    (
        safe_clamp(docked.0, ml.0, ml.0 + ml.2 - width),
        safe_clamp(docked.1, ml.1, ml.1 + ml.3 - ISLAND_H),
    )
}

/// 判定拖放位置应吸附的屏幕边（优先级：上 > 左 > 右；越界超出阈值视为自由位置）。
/// mon = (x， y， w， h) 显示器矩形；独立函数便于几何单测
pub(crate) fn detect_edge(
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
pub(crate) fn hidden_pos(
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
pub(crate) enum Slide {
    Jump,
    Out(u64),
    In(u64),
}

/// 滑入（显示/吸附落位）时长：减速进场
pub(crate) const SLIDE_SHOW_MS: u64 = 220;
/// 滑出（隐藏）时长：加速离场
pub(crate) const SLIDE_HIDE_MS: u64 = 160;

/// 程序化滑动窗口到位（to 为逻辑坐标）。动画期间产生的 Moved 事件由
/// IslandMotion.animating 屏蔽，落点由 programmed 消费，防止"移动 → Moved →
/// 再评估 → 再移动"的自触发循环；落点事件意外丢失时由 Moved 处理器按超时
/// 自复位（审查 3.7：去掉独立兜底线程）。按 16ms 步进插值缓动曲线，
/// 末步恰为落点（缓动函数 f(1)=1），保证 Moved 落点消费判定不受缓动影响
pub(crate) fn slide_to(
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
        let mut m = motion.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
pub(crate) enum IslandTarget {
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
pub(crate) fn island_transition(app: &tauri::AppHandle, target: IslandTarget) {
    let Some(win) = app.get_webview_window(ISLAND) else {
        return;
    };
    let motion = app.state::<Arc<Mutex<IslandMotion>>>();
    let edge = motion.lock().unwrap_or_else(std::sync::PoisonError::into_inner).edge.clone();

    match target {
        IslandTarget::Gone => {
            // 用户主动隐藏＝接管（五轮审查）：清启动回位标记——否则启动展示期内
            // 经托盘隐藏后，首个快照到达时 island_boot_settled 会违背用户意图
            // 把岛重新亮出（webview 隐藏期间仍在监听快照广播，命令照发）；
            // 独立锁块先行，遵循本函数「持锁不调 slide_to」纪律
            {
                let mut m = motion
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                m.boot_pending = false;
            }
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
            // 停靠位夹取进当前显示器（五轮审查）：换更窄的显示器后旧坐标可在
            // 屏外——Pill 召回路径早已 clamp，Peek 漏了同款（隐藏标签滑到屏外
            // ＝「贴边标签消失了」）；top 边生效 x、left/right 边生效 y
            let docked = clamp_docked(docked, ml, width);
            // 面板展开时贴边隐藏：先收拢到胶囊尺寸，锚定窗口底部的标签才会贴住屏幕边
            let _ = win.set_size(tauri::LogicalSize::new(width, ISLAND_H));
            {
                let mut m = motion.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
                let mut m = motion.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                m.hidden = false;
            }
            // 解除穿透立即生效：看护线程 90ms 粒度太慢，滑入途中收不到悬停
            let _ = win.set_ignore_cursor_events(false);
            // 贴边停靠：滑回停靠位（Out：减速进场）；自由位原地亮出，无需滑动。
            // 停靠位夹取进当前显示器（2026-10-03 审查修复）：换屏/改分辨率后旧
            // 坐标在屏外，不夹取则滑向屏外＝「召回点了没反应」（启动路径同款）
            if edge != "none" {
                let store = app.state::<Arc<Store>>();
                if let Some(docked) = saved_pos(&store) {
                    slide_to(
                        &win,
                        clamp_docked(docked, ml, width),
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
pub(crate) fn apply_snap(
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
    // 已知限制（2026-10-03 审查记录，暂不修）：混合 DPI 双屏（如 100%＋150%）
    // 拖放时用单一 current_monitor 的 scale 换算全局物理坐标，吸附判定可偏移
    // 十几像素——修复需跨显示器逐段换算，侵入大且场景边缘，记录在案
    let scale = mon.scale_factor();
    let ml = monitor_logical(&mon);
    let pos_l = phys_to_logical(pos, scale);
    let width = island_width(ml.2);
    let size = (width, ISLAND_H);

    let edge = detect_edge(pos_l, ml, size, SNAP_THRESHOLD);
    let (docked, hide) = match edge {
        "top" => (
            (
                safe_clamp(pos_l.0, ml.0, ml.0 + ml.2 - size.0),
                ml.1,
            ),
            autohide_enabled(store),
        ),
        "left" => (
            (ml.0, safe_clamp(pos_l.1, ml.1, ml.1 + ml.3 - size.1)),
            autohide_enabled(store),
        ),
        "right" => (
            (
                ml.0 + ml.2 - size.0,
                safe_clamp(pos_l.1, ml.1, ml.1 + ml.3 - size.1),
            ),
            autohide_enabled(store),
        ),
        _ => (pos_l, false),
    };
    {
        let mut m = motion.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        m.edge = edge.to_string();
        // 用户在启动展示期间拖放 = 主动接管位置，取消"数据就绪后回位"
        m.boot_pending = false;
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
pub(crate) fn lbutton_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    // SAFETY:GetAsyncKeyState 仅查询系统全局按键状态，无指针/生命周期风险；
    // 返回值短时置位语义与本用途（按住检测）兼容
    unsafe { GetAsyncKeyState(VK_LBUTTON.0.into()) as u16 & 0x8000 != 0 }
}

/// 光标是否落在顶部隐藏态标签矩形内（窗口内水平居中、底部 PEEK_TOP_H 逻辑像素）。
/// 看护线程 90ms 一次调停穿透开关的依据；窗口不缩窄（缩窗会触发 WebView2 重布局
/// 拉伸伪影，且宽窄跳变与 Moved 防误判协议互相干扰），改由穿透实现"窄标签"语义：
/// 穿透时两侧透明区域把点击还给下层窗口，光标进标签才解除（悬停区=可见区）
pub(crate) fn cursor_on_top_tab(win: &tauri::WebviewWindow) -> bool {
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
pub(crate) fn set_click_through(win: &tauri::WebviewWindow, on: bool, last: &mut bool) {
    if on == *last {
        return;
    }
    *last = on;
    let _ = win.set_ignore_cursor_events(on);
}


/// 岛启动定位（2026-10-08 启动原位展示改版，替代 2026-09-24 居中展示）：
/// 启动态胶囊直接出现在上次退出前正常态胶囊的位置（记忆坐标 island_pos 原位
/// 展示「启动中…」，无记忆则顶部居中 6px 兜底）；首个数据快照就绪后由
/// island_boot_settled 按上次状态收敛——贴边自动隐藏 → 从停靠位滑出隐藏，
/// 贴边不隐藏/自由位 → 原地不动（slide_to 等位守卫 no-op）。
/// 窗口在 tauri.conf.json 中不可见出生（visible=false）——否则窗口以默认出生位
/// （左上角）先可见、定位后才跳记忆位，闪现可见（用户实测）。因此此处必须同步
/// set_position 后亮出：slide_to 是异步线程会与 show 竞速，不能用于启动定位。
/// 定位必须带防误判标记（programmed/animating，同 slide_to Jump 档语义）：
/// 否则 set_position 产生的 Moved 事件会被看护线程误判为"拖放"，把距顶边
/// 仅 6px 的岛自动吸附隐藏
pub(crate) fn position_island(
    win: &tauri::WebviewWindow,
    store: &Store,
    motion: &Arc<Mutex<IslandMotion>>,
) {
    if let Ok(Some(mon)) = win.current_monitor() {
        let ml = monitor_logical(&mon);
        let width = island_width(ml.2);
        let remembered = saved_pos(store);
        // 原位展示（2026-10-08 改版）：x/y 均沿用记忆坐标，clamp_docked 夹取进
        // 当前显示器（防换屏/改分辨率后旧坐标出屏）；无记忆坐标（首次启动）
        // 兜底顶部水平居中 + 6px，原兜底不变
        let pos = match remembered {
            Some(p) => clamp_docked(p, ml, width),
            None => (ml.0 + (ml.2 - width) / 2, ml.1 + 6),
        };
        let to_phys = logical_to_phys(pos, mon.scale_factor());
        {
            let mut m = motion.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // 有记忆坐标才需要"数据就绪后回位"（贴边转工作态）；首次启动兜底位
            // 即默认位，原地不动
            m.boot_pending = remembered.is_some();
            // 同 slide_to Jump 档：声明程序化落点，看护线程据此消费 Moved 事件防误判
            m.programmed = Some(to_phys);
            m.animating = Some((std::time::Instant::now(), to_phys));
        }
        // 窗口尺寸先对齐自适应宽再定位：conf 出生宽（380）与自适应宽可能不同
        // （如 1920 屏 → 576），不同对齐则展示期胶囊中心偏离屏幕中心
        // （差值 = 宽度差的一半），数据就绪 set_size 时还会肉眼可见地微跳一下
        let _ = win.set_size(tauri::LogicalSize::new(width, ISLAND_H));
        let _ = win.set_position(tauri::PhysicalPosition::new(to_phys.0, to_phys.1));
        log::debug!("[岛] 启动定位：原位落点 {pos:?}（记忆 {remembered:?}），数据就绪后按上次状态回位");
    }
    // 统一亮出（conf 出生不可见）：显示器获取失败时以出生位显示兜底——
    // 可见降级优于永久隐藏（宪法红线④）
    let _ = win.show();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 窄逻辑屏防 panic（五轮审查回归锁）：逻辑宽 < 岛宽 380（如 800px 物理
    /// ＋250% 缩放 → 逻辑 320）时 `ml.0 + ml.2 - width` 小于 `ml.0`，原生
    /// i32::clamp 在 min>max 上直接 panic——safe_clamp 退化为贴 lo 不崩；
    /// clamp_docked 两条消费路径（Pill 召回 / Peek 隐藏落点）同受保护
    #[test]
    fn test_safe_clamp_narrow_monitor() {
        // 正常区间行为与原生 clamp 一致
        assert_eq!(safe_clamp(500, 0, 1000), 500);
        assert_eq!(safe_clamp(-30, 0, 1000), 0);
        assert_eq!(safe_clamp(2000, 0, 1000), 1000);
        // min>max（窄屏）：不 panic，贴 lo
        assert_eq!(safe_clamp(500, 0, -60), 0);
        // clamp_docked 在 320 逻辑宽（< 岛宽 380）的显示器上不 panic，
        // 且两条轴都落在合法区间（x 贴左缘，y 正常夹取）
        let ml = (0, 0, 320, 240);
        let (x, y) = clamp_docked((100, 80), ml, island_width(320)); // width=380 > 320
        assert_eq!((x, y), (0, 80));
        // 常规屏：换更窄显示器后旧坐标被夹回屏内（x 越界夹回，y 合法不动）
        let ml = (0, 0, 1280, 720);
        let (x, y) = clamp_docked((1500, 650), ml, island_width(1280)); // 384
        assert_eq!((x, y), (1280 - 384, 650));
    }
}
