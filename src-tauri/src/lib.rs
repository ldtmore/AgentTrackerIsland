// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
pub mod commands;
pub mod collector;
pub mod island;
pub mod tray;
pub mod localapi;
pub mod logging;
pub mod modelcat;
pub mod provider;
pub mod state;
pub mod store;

use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering;
use std::time::Duration;

use tauri::{Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::state::service::Aggregator;
use crate::store::Store;

// 四轮审查拆分：命令/岛运动学/托盘三域自本文件迁出（纯移动），此处整体
// 引入供 run() 的 invoke_handler 与 setup 闭包引用
use crate::commands::*;
use crate::island::*;
use crate::tray::*;

/// 岛窗口标签（tauri.conf.json 中定义）
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // 单实例守卫（2026-10-03 审查新增，官方建议注册在最前）：双开在 Windows
        // 常见（开机自启＋手点图标），两实例会双采双写、托盘图标重复；二次启动
        // 转发到此回调后自行退出——岛窗隐藏时召回亮出（用户点了图标的意图是
        // "看到它"），已可见则不打扰
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window(ISLAND) {
                if !w.is_visible().unwrap_or(false) {
                    toggle_island(app);
                }
            }
        }))
        // （2026-09-17 审查 3.1）tauri-plugin-opener 全项目零引用，已随依赖移除
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        // 保存对话框（报表 CSV 导出让用户自选目录；权限仅 dialog:allow-save）
        .plugin(tauri_plugin_dialog::init())
        // 辅助窗口几何记忆（07-UX 1.2）：设置/报表/额度/会话/关于五窗口的尺寸、
        // 位置与最大化状态跨重启记忆。岛与托盘菜单在 denylist——岛有自有几何
        // 体系（贴边/滑出/记忆位），托盘菜单高度由内容实测回写，都不可被接管；
        // state_flags 显式剔除 VISIBLE 位——插件恢复可见态会直接破坏「常驻隐藏、
        // show 唤起」的窗口模型。恢复发生在窗口创建时（on_window_ready），
        // 保存发生在应用退出（RunEvent::Exit）——窗口是 hide 非 destroy，
        // 退出保存即正确时机。恢复坐标的出屏兜底见 clamp_aux_windows_on_boot
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::SIZE
                        | tauri_plugin_window_state::StateFlags::POSITION
                        | tauri_plugin_window_state::StateFlags::MAXIMIZED,
                )
                .with_denylist(&["island", "tray-menu"])
                .build(),
        )
        // 全局快捷键（07-UX 2.3）：Ctrl+Alt+I 显隐灵动岛。注册/注销与设置键
        // hotkey_toggle_island 联动（setup 初始注册＋set_setting 变更跟进，
        // 见 tray::apply_island_hotkey）；此处只挂按下沿分发器——转后台线程执行
        // toggle（与托盘 toggle 菜单项同款主线程减负，内部含滑动动画与窗口操作）。
        // 纯 Rust 侧使用：无前端包、无 capability；不弹窗不抢焦点，红线⑤无虞
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        let app = app.clone();
                        std::thread::spawn(move || toggle_island(&app));
                    }
                })
                .build(),
        )
        // 系统通知（07-UX 2.6）：会话等待输入 opt-in 通知（默认关）＋首次关闭
        // 到托盘的一次性提示。纯 Rust 侧使用：无前端包、无 capability；
        // 展示行为（勿扰/焦点辅助）遵循系统通知设置，本应用不做额外打扰策略
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            focus_session,
            get_settings,
            set_setting,
            provider_kinds,
            provider_kind_entries,
            display_constants,
            quota_pace,
            budget_usage,
            year_heatmap,
            provider_overview,
            provider_account_create,
            provider_account_update,
            provider_account_delete,
            provider_account_set_enabled,
            provider_account_set_in_island,
            provider_account_reorder,
            provider_account_test,
            provider_account_copy_key,
            provider_account_refresh,
            env_creds_probe,
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
            export_report_csv,
            open_file_location,
            open_app_dir,
            show_report_window,
            show_sessions_window,
            show_quotas_window,
            show_settings_window,
            island_peek,
            island_refresh,
            island_dock_state,
            island_boot_settled,
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
            // 全局快捷键初始注册（07-UX 2.3）：键缺失＝默认启用 Ctrl+Alt+I，
            // 空值＝关闭。启动注册失败（组合被占用）仅落日志、功能保持关闭，
            // 不阻塞启动——设置页再次开启时会重试并 toast 提示
            {
                let v = store
                    .get_setting("hotkey_toggle_island")
                    .unwrap_or_else(|| crate::tray::HOTKEY_ISLAND_COMBO.into());
                if let Err(e) = crate::tray::apply_island_hotkey(app.handle(), &v) {
                    log::warn!("[快捷键] 初始注册未生效（功能保持关闭）：{e}");
                }
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

            // 定位：启动态在记忆坐标原位展示「启动中…」（数据就绪后经
            // island_boot_settled 按上次状态收敛）；无记忆坐标时兜底位即默认位，
            // 原地不动
            position_island(&win, store.as_ref(), &motion);

            // 窗口移动事件：程序化滑动的落点被消费忽略；用户拖拽则记录待评估位置
            {
                let motion2 = motion.clone();
                win.on_window_event(move |ev| {
                    let tauri::WindowEvent::Moved(pos) = ev else {
                        return;
                    };
                    // 中毒自恢复（五轮审查，与看护线程/适配器锁同纪律）：本处理器
                    // 跑在主线程事件循环，motion 锁中毒时 unwrap 会让全应用崩溃
                    let mut m = motion2
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                    // 用户拖拽：打断在播动画，交给看护线程防抖评估。
                    // pending 写入与清 programmed 同锁块完成（五轮审查）：原先
                    // drop 后二次加锁，间隙内看护线程可 take 走旧 pending、对旧
                    // 坐标做一次多余贴边评估
                    m.programmed = None;
                    m.pending = Some((std::time::Instant::now(), pos.x, pos.y));
                    drop(m);
                    SLIDE_GEN.fetch_add(1, Ordering::Relaxed);
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
                        // 单轮 panic 不杀线程（四轮审查）：看护线程死＝贴边评估与
                        // 穿透调停全部停摆，岛可能卡在隐藏态无法唤回（对齐聚合 tick 纪律）
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            // motion 锁中毒不连锁 panic（与 Store::lock_conn 同款）：
                            // 中毒说明曾有 panic 发生在持锁期间，取回数据续跑比
                            // 每 90ms 连环 panic 刷日志更可取
                            let fire = {
                                let mut m = motion2
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                                let m = motion2
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                m.edge == "top" && m.hidden
                            };
                            let ct_on = if ct_want {
                                cursor_on_top_tab(&win2)
                            } else {
                                false
                            };
                            set_click_through(&win2, ct_on, &mut last_ct);
                        }));
                        if let Err(payload) = result {
                            log::error!(
                                "贴边看护 panic（本轮跳过，线程续跑）：{}",
                                logging::panic_payload_str(payload)
                            );
                        }
                    }
                });
            }

            // 启动恢复（2026-10-08 启动原位展示改版）：贴边自动隐藏用户不再在此
            // 立即瞬移滑出——原逻辑会在定位后 10ms 内覆盖启动展示（实测 17ms，
            // 「启动中…」完全不可见）；现启动期保持原位展示，数据就绪后经
            // island_boot_settled 统一回位（贴边 → Peek 动画滑出；其余 → 原地
            // 不动，等位守卫 no-op）。此时 webview 未挂载无需发事件：settled
            // 触发时前端必然已挂载并监听（它就是快照接收方），island-dock 事件必达
            let edge = motion
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .edge
                .clone();
            if edge != "none" && autohide_enabled(store.as_ref()) {
                log::debug!("[岛] 启动贴边恢复延后：启动期原位展示，数据就绪后滑出 edge={edge}");
            }

            // 设置/会话/报表/额度/关于/托盘菜单窗口：关闭即隐藏（而非销毁），保证托盘可反复唤起。
            // 首次关闭到托盘的一次性提示（07-UX 2.6 并入项，落键 tray_hint_done）：
            // 用户首次关窗（往往以为是退出）时告知「应用仍在托盘运行」。一次性教育
            // 不属常驻打扰，不受 notify_waiting 开关约束（红线的 opt-in 管的是
            // 重复性通知）；托盘菜单是纯弹出面板，Alt+F4 不属「关窗到托盘」认知
            // 场景，不参与提示；tray_hint_done 是 Rust 内部键，不经前端写入故不入
            // 设置键白名单
            for label in ["settings", "sessions", "report", "quotas", "about", "tray-menu"] {
                if let Some(w) = app.get_webview_window(label) {
                    let w2 = w.clone();
                    let hint = if label != "tray-menu" {
                        Some((store.clone(), app.handle().clone()))
                    } else {
                        None
                    };
                    w.on_window_event(move |ev| {
                        if let tauri::WindowEvent::CloseRequested { api, .. } = ev {
                            api.prevent_close();
                            let _ = w2.hide();
                            if let Some((store2, app2)) = &hint {
                                if store2.get_setting("tray_hint_done").as_deref() != Some("1") {
                                    let _ = app2
                                        .notification()
                                        .builder()
                                        .title("去你的岛")
                                        .body("窗口已隐藏到托盘，应用仍在运行——可从托盘图标随时打开")
                                        .show();
                                    store2.set_setting("tray_hint_done", "1");
                                    log::debug!("[托盘] 首次关闭提示已发出，落键 tray_hint_done");
                                }
                            }
                        }
                    });
                }
            }

            // 辅助窗口几何兜底（07-UX 1.2）：窗口状态插件已在此前窗口创建时恢复
            // 记忆几何（插件 setup 先于配置窗口创建），此处对恢复结果做一次
            // 拔屏/改缩放后的出屏钳制——放到 setup 尾部保证恢复已完成
            clamp_aux_windows_on_boot(app.handle());

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
            // 审查 3.8：status_events 每工具调用一条，永不清理会让库无限膨胀）。
            // 每日接力节拍在额度工作线程（provider/worker.rs，2026-09-29 审查
            // 修复：原仅启动清理一次，常驻不重启场景库无限增长）。
            // 挪后台线程执行（五轮审查）：清理虽已分批＋批间释放 Mutex，「长保留
            // 周期改短」的首轮可达数千批——同步跑在 setup 闭包里会把主线程事件
            // 循环起步阻塞数秒，岛窗「启动中…」首帧被推迟；挪走无语义变化
            {
                let store2 = store.clone();
                std::thread::spawn(move || {
                    let removed = provider::worker::run_scheduled_cleanup(&store2);
                    if removed > 0 {
                        let cleanup_days = store2
                            .get_setting("cleanup_days")
                            .and_then(|v| v.parse::<i64>().ok())
                            .unwrap_or(365);
                        log::info!("启动清理：按保留 {cleanup_days} 天删除 {removed} 条过期数据");
                    }
                });
            }

            // 额度调度工作线程（2026-09-29 审查修复）：外呼/错峰/退避全部脱离
            // 聚合 tick；通知通道承接额度页「查询」成功后的退避复位（#16）
            let (quota_ntx, quota_nrx) = std::sync::mpsc::sync_channel::<String>(16);
            app.manage(QuotaRefreshNotify(quota_ntx));
            provider::worker::spawn_quota_worker(store.clone(), quota_nrx);

            // 本地只读 API（#25，默认关）：开启时绑定 127.0.0.1，供脚本/小组件
            // 读取观测数据；绑定失败留痕不阻塞应用；开关变更重启生效
            if store.get_setting("local_api_enabled").as_deref() == Some("1") {
                let port = store
                    .get_setting("local_api_port")
                    .and_then(|v| v.parse::<u16>().ok())
                    .unwrap_or(6737);
                localapi::spawn(store.clone(), port);
            }

            spawn_aggregator(app.handle().clone(), store);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
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
        // store 克隆一份自用（原份移入聚合器）：等待通知开关（notify_waiting）
        // 每 tick 现读——与 agents_enabled 的每 tick 读同款，SQLite 点查微秒级
        let mut agg = Aggregator::new(store.clone());
        // 快轮信号在 agg 被移入主环前取出（信号是自足的声明，不依赖 agg 存活）
        let signals = agg.hot_signals();
        // 容量 1 的同步通道：try_send 满即丢，天然合并唤醒风暴
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        {
            // 快轮线程：恒定 1s 采样；last 初始全 None，「从无到有」首现即唤醒。
            // 单轮 panic 不杀线程（四轮审查）：快轮是「≤2s 首信号」唯一通道，
            // 线程死＝秒级感知静默退化为 5/10s 慢档且无任何提示（对齐聚合 tick 纪律）
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut last: Vec<Option<crate::collector::engine::SignalValue>> =
                    vec![None; signals.len()];
                loop {
                    std::thread::sleep(Duration::from_millis(HOT_POLL_MS));
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        for (i, s) in signals.iter().enumerate() {
                            let v = s.sample();
                            if v != last[i] {
                                let _ = tx.try_send(());
                            }
                            last[i] = v;
                        }
                    }));
                    if let Err(payload) = result {
                        log::error!(
                            "快轮采样 panic（本轮跳过，线程续跑）：{}",
                            logging::panic_payload_str(payload)
                        );
                    }
                }
            });
        }
        let mut last_sig: Option<u64> = None;
        let mut last_tick = std::time::Instant::now();
        // 初始按"无会话"档进入：首轮立即 tick，随后由快照驱动档位切换
        let (mut any_active, mut has_sessions) = (false, false);
        // 会话等待通知（07-UX 2.6）的边沿与去重状态：上轮各会话状态（判「进入
        // Waiting」边沿）＋各会话上次提醒时刻（30 分钟去重）。均内存态，重启清零
        // ——重启后仍处等待的会话会再提醒一次，符合「重启后想知道还有什么卡着」
        let mut prev_states: std::collections::HashMap<String, crate::state::SessionState> =
            std::collections::HashMap::new();
        let mut wait_notified: std::collections::HashMap<String, std::time::Instant> =
            std::collections::HashMap::new();
        // 托盘告警红点（07-UX 3.4）的边沿状态：None＝启动后未定，首个快照即落定
        // （启动时就处于出错态则直接亮红点，不漏初始状态）
        let mut last_tray_attention: Option<bool> = None;
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
            // 护栏命中改「补等剩余间隔」而非丢弃（五轮审查 P1）：快轮唤醒是
            // 变化沿一次性资源（值不再变化不会重发），原先 continue 把唤醒
            // 直接吞掉——变化沿恰落在 250ms 护栏窗内时（无会话档约 2.5% 概率）
            // 本轮只能等自然到期，「用户刚启动 Agent」的首信号从 ~2s 退化到
            // 10s，违背「首信号 ≤2s」的设计目标；补等后唤醒被排队，语义仍是
            // 「两次 tick 至少间隔 MIN_TICK_GAP」
            let since_tick = last_tick.elapsed();
            if since_tick < Duration::from_millis(MIN_TICK_GAP_MS) {
                std::thread::sleep(Duration::from_millis(MIN_TICK_GAP_MS) - since_tick);
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
                    // 托盘告警红点（07-UX 3.4）：出错态（任一会话 error 或额度
                    // 耗尽）边沿切换图标，恢复常亮。两者都是快照内容的投影，
                    // 内容不变时不翻转，挂在广播判定同处即可
                    let attention = snap.island == crate::state::IslandState::AnyError
                        || snap.quota_exhausted;
                    if last_tray_attention != Some(attention) {
                        log::debug!("[托盘] 告警红点边沿：{:?} → {}", last_tray_attention, attention);
                        last_tray_attention = Some(attention);
                        crate::tray::apply_tray_attention(&app, attention);
                    }
                    // 会话等待通知（07-UX 2.6，默认关）：每 tick 都要跑——快照
                    // 签名去重只省 emit，状态沿不能漏。快照本身只含已启用 Agent
                    // 的会话（禁用适配器被聚合器跳过），「仅已启用参与」天然满足
                    if store.get_setting("notify_waiting").as_deref() == Some("1") {
                        for s in &snap.sessions {
                            let now = std::time::Instant::now();
                            if should_notify_waiting(
                                prev_states.get(&s.id).copied(),
                                s.state,
                                wait_notified.get(&s.id).copied(),
                                now,
                            ) {
                                wait_notified.insert(s.id.clone(), now);
                                // 通知失败不重试不落库（系统侧拒绝属用户环境状态，
                                // 30 分钟去重窗口自然兜住重试节奏）
                                let _ = app
                                    .notification()
                                    .builder()
                                    .title("去你的岛")
                                    .body(format!("{} 有会话等待输入", agent_display_name(&s.agent)))
                                    .show();
                                log::debug!("[通知] 等待输入已提醒：{}（{}）", s.id, s.agent);
                            }
                        }
                    }
                    // 状态缓存推进与清理：无论开关与否都更新（关→开瞬间不把存量
                    // 等待误报为边沿）；消失会话的缓存随拍对齐聚合器 last_states
                    // 的清理语义，防长期运行泄漏
                    for s in &snap.sessions {
                        prev_states.insert(s.id.clone(), s.state);
                    }
                    prev_states.retain(|k, _| snap.sessions.iter().any(|s| &s.id == k));
                    wait_notified.retain(|k, v| {
                        prev_states.contains_key(k) && v.elapsed() < WAIT_NOTIFY_DEDUP
                    });
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

/// 等待通知的同会话去重窗口（07-UX 2.6）：30 分钟内离开再进入等待不重复提醒
const WAIT_NOTIFY_DEDUP: Duration = Duration::from_secs(30 * 60);

/// 等待通知判定（07-UX 2.6，纯函数便于单测）：「进入 Waiting 的边沿」＋同会话
/// 30 分钟去重。首轮观察（prev=None）即 Waiting 也算边沿——应用重启后对仍卡着
/// 的会话补一声提醒，符合「重启后想知道还有什么卡着」的预期；
/// prev==Waiting（持续等待）不算边沿，杜绝每 tick 重复打扰
fn should_notify_waiting(
    prev: Option<crate::state::SessionState>,
    cur: crate::state::SessionState,
    last_notified: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    if cur != crate::state::SessionState::Waiting {
        return false;
    }
    if prev == Some(crate::state::SessionState::Waiting) {
        return false;
    }
    !matches!(last_notified, Some(t) if now.duration_since(t) < WAIT_NOTIFY_DEDUP)
}

/// Agent 显示名（07-UX 2.6 通知文案用）：与前端 shared/types.ts 的 AGENT_DEFS
/// label 同源双端手工维护（仿 AGENT_DISPLAY_ORDER/HOOKS_AGENTS 先例）。
/// 未知 id 兜底回 id 本身——新家接入漏登记不崩文案，仅显示机器名
fn agent_display_name(id: &str) -> &str {
    match id {
        "zcode" => "ZCode",
        "claude-code" => "Claude Code",
        "codex" => "Codex",
        "kimi-code" => "Kimi Code",
        "opencode" => "OpenCode",
        "mimo-code" => "MiMo Code",
        "gemini" => "Gemini CLI",
        "qwen-code" => "Qwen Code",
        "openclaw" => "OpenClaw",
        // 官方全名（2026-10-09 与前端 AGENT_DEFS 同源修正：裸 Hermes 与
        // NousResearch 的 LLM 系列重名；Copilot CLI 与已废弃旧工具混淆）
        "hermes" => "Hermes Agent",
        "copilot" => "GitHub Copilot CLI",
        "goose" => "Goose",
        "codebuddy" => "CodeBuddy Code",
        "qoder" => "Qoder",
        "aider" => "Aider",
        "workbuddy" => "WorkBuddy",
        other => other,
    }
}

/// 当前 Unix 毫秒（广播时间戳回填用）
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 等待通知判定（07-UX 2.6）：边沿触发、持续等待不重复、30 分钟去重
    #[test]
    fn test_should_notify_waiting() {
        let now = std::time::Instant::now();
        use crate::state::SessionState as S;
        // 进入等待的边沿（idle/working/error → waiting）触发
        assert!(should_notify_waiting(Some(S::Idle), S::Waiting, None, now));
        assert!(should_notify_waiting(Some(S::Working), S::Waiting, None, now));
        assert!(should_notify_waiting(Some(S::Error), S::Waiting, None, now));
        // 首轮观察即等待（prev=None）也算边沿：重启后对仍卡着的会话补提醒
        assert!(should_notify_waiting(None, S::Waiting, None, now));
        // 持续等待（prev=Waiting）不是边沿：杜绝每 tick 重复打扰
        assert!(!should_notify_waiting(Some(S::Waiting), S::Waiting, None, now));
        // 非 Waiting 一律不发
        assert!(!should_notify_waiting(None, S::Working, None, now));
        assert!(!should_notify_waiting(Some(S::Waiting), S::Idle, None, now));
        // 30 分钟去重：窗口内再进入等待被拦下，窗口外放行
        let notified = now - Duration::from_secs(10 * 60);
        assert!(!should_notify_waiting(Some(S::Idle), S::Waiting, Some(notified), now));
        let stale = now - (WAIT_NOTIFY_DEDUP + Duration::from_secs(1));
        assert!(should_notify_waiting(Some(S::Idle), S::Waiting, Some(stale), now));
    }

    /// Agent 显示名：已知 id 出正式名，未知 id 兜底回机器名不崩文案
    #[test]
    fn test_agent_display_name() {
        assert_eq!(agent_display_name("claude-code"), "Claude Code");
        assert_eq!(agent_display_name("zcode"), "ZCode");
        assert_eq!(agent_display_name("no-such-agent"), "no-such-agent");
    }

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
