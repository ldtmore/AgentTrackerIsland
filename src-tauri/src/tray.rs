//! 托盘域（2026-10-03 四轮审查拆分自 lib.rs，纯移动零逻辑变更）：托盘图标
//! 构建/左右键动作分发/自绘菜单窗定位，及岛显隐切换与菜单动作 command。
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager};
use crate::commands::show_aux_window;
use crate::island::{island_transition, IslandMotion, IslandTarget, ISLAND};
use crate::store::Store;


// ===== 托盘菜单（webview 自绘，M1-9） =====
// 原生托盘菜单（muda → Win32 弹出菜单）的行高/间距/字体全部由系统决定，muda 无
// 样式 API（已核对 0.19.3 源码，图标位图亦硬编码 16×16），"更舒展精致"在原生菜单
// 内无解。改为无边框置顶小窗（#tray-menu，tauri.conf.json）自绘：行高/间距/圆角/
// 动效/双主题全部可控，样式只引用岛面板的 CSS 变量体系。菜单动作与岛显隐共用
// 同一批内部函数（单一事实源，见 tray_menu_action）；显隐语义不变（隐藏=彻底
// 消失/显示=临时召唤，玩游戏前一键隐藏玩完召回的场景照旧），见 toggle_island。

/// 托盘左键动作（设置项 tray_left_action）：none=无操作（默认）｜toggle=显隐灵动岛｜
/// menu=打开托盘菜单。右键恒为打开托盘菜单，不进设置（Windows 托盘通用惯例）
pub(crate) fn tray_left_action(store: &Store) -> &'static str {
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
pub(crate) fn tray_menu_pos(
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
pub(crate) fn monitor_at(app: &tauri::AppHandle, pt: (f64, f64)) -> Option<tauri::Monitor> {
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
pub(crate) fn show_tray_menu(app: &tauri::AppHandle, rect: tauri::Rect) {
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
pub(crate) fn build_tray(app: &tauri::App) -> anyhow::Result<()> {
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    TrayIconBuilder::with_id(TRAY_ID)
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

// ===== 托盘状态红点（07-UX 3.4） =====
// 出错态（任一会话 error 或额度耗尽）托盘图标切红点变体，恢复常亮——岛被隐藏
// 时托盘是唯一常驻指示。仅红点不闪烁（与岛动效语言一致的克制表达）。
// 变体不入库第二套二进制图标资源：运行时从应用图标 RGBA 原地合成（同源不漂移，
// 分辨率随系统 DPI），红点几何提纯为纯函数便于单测

/// 托盘图标标识（提常量：告警切换需按 id 取回句柄）
pub(crate) const TRAY_ID: &str = "at-tray";

/// 红点主色（高饱和中红）：任务栏浮于系统底色之上，应用内 --danger 双主题
/// 分档（#f87171/#b91c1c）都不适合直印，此值在明暗任务栏下均可辨；
/// 描边深灰增强浅色任务栏下的轮廓
const ALERT_RED: (u8, u8, u8) = (232, 58, 58);
/// 描边色（深灰）
const ALERT_RING: (u8, u8, u8) = (30, 30, 30);

/// 红点几何参数（纯函数，由图标尺寸推导便于单测）：
/// 返回（圆心 cx、cy、核心半径 r、描边宽 ring）。半径按短边 22%——32px 源下
/// 核心约 7px，缩到 16px 托盘位仍有 ~3.5px 可辨；描边随短边微缩放；圆心内缩
/// 边距保证红点不贴死图标角落（非正方形图标恒锚右下角）
fn dot_geometry(w: u32, h: u32) -> (f32, f32, f32, f32) {
    let side = w.min(h) as f32;
    let r = side * 0.22;
    let ring = 1.0 + side * 0.02;
    let margin = r * 0.28;
    let cx = w as f32 - margin - r;
    let cy = h as f32 - margin - r;
    (cx, cy, r, ring)
}

/// 像素中心 (x, y)（浮点坐标，调用方已加 0.5）落在红点（核心＋描边）内的
/// 着色判定（纯函数）：返回 Some((r, g, b, 覆盖度 0~1))，None＝不涉红点。
/// 核心缘与描边缘各 1px 线性羽化抗锯齿（托盘位缩放采样平滑，高分屏 1:1 也不起锯齿）
fn alert_dot_pixel(x: f32, y: f32, w: u32, h: u32) -> Option<(u8, u8, u8, f32)> {
    let (cx, cy, r, ring) = dot_geometry(w, h);
    let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
    // 覆盖度：距边缘半像素内线性衰减到 0（1px 羽化带）
    let feather = |d: f32, edge: f32| (edge + 0.5 - d).clamp(0.0, 1.0);
    if d <= r + 0.5 {
        Some((ALERT_RED.0, ALERT_RED.1, ALERT_RED.2, feather(d, r)))
    } else if d <= r + ring + 0.5 {
        Some((
            ALERT_RING.0,
            ALERT_RING.1,
            ALERT_RING.2,
            feather(d, r + ring),
        ))
    } else {
        None
    }
}

/// 告警图标缓存：运行时从应用图标合成一次常驻复用。Option 包一层——
/// default_window_icon 拿不到时记 None，后续调用复用结论不再重试
/// （图标缺失属构建配置问题，非运行态波动）
static ALERT_ICON: std::sync::OnceLock<Option<tauri::image::Image<'static>>> =
    std::sync::OnceLock::new();

/// 从应用图标合成右下角带红点的告警变体（07-UX 3.4）：与常亮图标同一像素源，
/// 双态天然同源（无第二套二进制资源可漂移），分辨率随系统 DPI。直写 alpha
/// 合成（straight RGBA，over 语义），羽化覆盖度即源不透明度
fn alert_icon(app: &tauri::AppHandle) -> Option<tauri::image::Image<'static>> {
    ALERT_ICON
        .get_or_init(|| {
            let base = app.default_window_icon()?;
            let (w, h) = (base.width(), base.height());
            let mut rgba = base.rgba().to_vec();
            for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let x = (i % w as usize) as f32 + 0.5;
                let y = (i / w as usize) as f32 + 0.5;
                if let Some((sr, sg, sb, cov)) = alert_dot_pixel(x, y, w, h) {
                    // straight-alpha over：src 覆盖度 cov 叠在原像素（可能透明）上
                    let sa = cov;
                    let da = px[3] as f32 / 255.0;
                    let oa = sa + da * (1.0 - sa);
                    let mix = |s: u8, d: u8| -> u8 {
                        if oa <= 0.0 {
                            0
                        } else {
                            ((s as f32 * sa + d as f32 * da * (1.0 - sa)) / oa).round() as u8
                        }
                    };
                    px[0] = mix(sr, px[0]);
                    px[1] = mix(sg, px[1]);
                    px[2] = mix(sb, px[2]);
                    px[3] = (oa * 255.0).round() as u8;
                }
            }
            Some(tauri::image::Image::new_owned(rgba, w, h))
        })
        .clone()
}

/// 岛外告警切换（07-UX 3.4）：alert=true 切红点变体，false 恢复常亮图标。
/// 失败仅落日志不重试——装饰性提示不值得为它打扰；调用方（聚合循环）做
/// 边沿检测，本函数只在状态翻转时被调
pub(crate) fn apply_tray_attention(app: &tauri::AppHandle, alert: bool) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let icon = if alert {
        match alert_icon(app) {
            Some(i) => i,
            None => {
                log::debug!("[托盘] 告警图标合成失败（缺默认图标），保持当前图标");
                return;
            }
        }
    } else {
        match app.default_window_icon() {
            Some(i) => i.clone(),
            None => return,
        }
    };
    if let Err(e) = tray.set_icon(Some(icon)) {
        log::warn!("[托盘] 告警图标切换失败：{e}");
    } else {
        log::debug!("[托盘] 告警红点：{}", if alert { "亮起" } else { "恢复常亮" });
    }
}

/// 岛显隐全局快捷键的固定组合（07-UX 2.3）：本期只做「关闭／默认组合」两档
/// （YAGNI，自定义组合输入列后续增强档）。设置键 hotkey_toggle_island 存组合串，
/// 空值＝未启用，键缺失＝默认启用（前端 widgets.tsx 的 HOTKEY_ISLAND_DEFAULT 同源）
pub(crate) const HOTKEY_ISLAND_COMBO: &str = "ctrl+alt+i";

/// 应用岛显隐全局快捷键（07-UX 2.3）：按设置值同步实际注册状态——空值＝注销；
/// 非空必须等于固定组合（白名单，防任意组合串透传插件）。先注销旧注册再注册
/// 新值（unregister 未注册时的报错属预期，忽略）。返回 Err（组合被其他程序
/// 占用/值非法）时由 set_setting 报错回滚且不落库；调用方在失败路径应尽力
/// 回注旧值，防「库说开着、实际没注册」的假开
pub(crate) fn apply_island_hotkey(app: &tauri::AppHandle, value: &str) -> Result<(), String> {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};

    let gs = app.global_shortcut();
    // 组合串解析一次（注销/注册共用）；解析失败＝白名单外的脏值
    let combo: Shortcut = HOTKEY_ISLAND_COMBO
        .parse()
        .map_err(|_| format!("快捷键组合串非法：{HOTKEY_ISLAND_COMBO}"))?;
    let _ = gs.unregister(combo);
    if value.trim().is_empty() {
        return Ok(()); // 关闭档：注销即完成
    }
    if value.trim() != HOTKEY_ISLAND_COMBO {
        return Err(format!(
            "不支持的快捷键组合：{value}（当前仅支持 {HOTKEY_ISLAND_COMBO}）"
        ));
    }
    gs.register(combo)
        .map_err(|e| format!("快捷键注册失败（可能已被其他程序占用）：{e}"))
}

/// 报表目标高度（逻辑像素，与 tauri.conf.json 的 report.height 同源同值）。
/// 2026-09-24 定为宽 900 的 4:3（与设置页 800×600 同比例）：默认比屏幕小一档，
/// 托盘切换灵动岛显隐（"隐藏"=彻底消失，"显示"=临时召唤）。
/// 贴边隐藏态的窗口仍在屏外"可见"（is_visible=true）——隐藏是整窗 hide；
/// 显示时贴边停靠滑回停靠位亮相（summon：3s 无操作自动收回，鼠标移入即
/// 取消，见 App.tsx island-summon），自由摆放原地亮出、无自动收起。
/// 编排细节全部在 island_transition（显隐唯一入口）
pub(crate) fn toggle_island(app: &tauri::AppHandle) {
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
pub(crate) fn tray_menu_action(app: tauri::AppHandle, action: String) -> Result<(), String> {
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
        "quotas" => {
            show_aux_window(&app, "quotas");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 红点几何（07-UX 3.4）：核心／描边／羽化／无关区四档判定，
    /// 32px 与 16px 两档尺寸都验（托盘位缩放两端）
    #[test]
    fn test_alert_dot_pixel() {
        for side in [32u32, 16u32] {
            let (cx, cy, r, ring) = dot_geometry(side, side);
            // 圆心＝红核全覆盖
            let (cr, cg, cb, cov) = alert_dot_pixel(cx, cy, side, side).unwrap();
            assert_eq!((cr, cg, cb), ALERT_RED);
            assert!((cov - 1.0).abs() < 1e-6, "圆心覆盖度应为 1");
            // 核心内 1px＝红核全覆盖
            let (_, _, _, cov) = alert_dot_pixel(cx + r - 1.0, cy, side, side).unwrap();
            assert!((cov - 1.0).abs() < 1e-6);
            // 核心缘外 0.25px＝羽化带（0<覆盖<1，仍属红核色与分支）
            let (cr2, _, _, cov2) = alert_dot_pixel(cx + r + 0.25, cy, side, side).unwrap();
            assert_eq!((cr2, cg, cb), ALERT_RED, "羽化带仍属红核色");
            assert!(cov2 > 0.0 && cov2 < 1.0);
            // 描边带中点＝深灰全覆盖
            let (cr3, cg3, cb3, cov3) =
                alert_dot_pixel(cx + r + ring * 0.5, cy, side, side).unwrap();
            assert_eq!((cr3, cg3, cb3), ALERT_RING);
            assert!((cov3 - 1.0).abs() < 1e-6);
            // 描边外远离＝None（图标左上角必不在红点内）
            assert!(alert_dot_pixel(1.0, 1.0, side, side).is_none());
        }
    }

    /// 几何约束：红点整体落在图标内（32px 源），描边外缘不越界出图标
    #[test]
    fn test_dot_geometry_within_icon() {
        let (cx, cy, r, ring) = dot_geometry(32, 32);
        assert!(cx - r - ring >= 0.0 && cy - r - ring >= 0.0, "红点不得越出左上");
        assert!(cx + r + ring <= 32.0 && cy + r + ring <= 32.0, "红点不得越出右下");
    }
}
