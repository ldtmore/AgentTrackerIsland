/**
 * 深浅双主题（M1-4）：模式存 app_settings 的 theme 键（system/dark/light，默认 system），
 * 解析结果写 document.documentElement.dataset.theme，CSS 变量按 [data-theme] 切换色板。
 *
 * "跟随系统"信号链（已核对本地 wry 0.55 / tauri-runtime-wry 2.11 源码）：
 * 系统主题变化 → tao 窗口 ThemeChanged → 运行时把新主题推给 WebView2
 * （SetPreferredColorScheme）→ 网页内 matchMedia 触发 change。全程前端可感知，零 Rust 参与。
 */
import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";

/** 主题模式：system=跟随系统（默认），dark/light=自选 */
export type ThemeMode = "system" | "dark" | "light";

/** 解析后的实际主题（CSS 只认 dark/light 两种色板） */
export type ResolvedTheme = "dark" | "light";

/** 设置存储键与缺省值（与 Rust 端无关，纯前端约定） */
export const THEME_KEY = "theme";
export const THEME_DEFAULT: ThemeMode = "system";

/** 系统深浅色查询（WebView2 的 PreferredColorScheme 由 wry 维护，随系统实时更新） */
const SCHEME_QUERY = window.matchMedia("(prefers-color-scheme: dark)");

/** 收窄任意值为合法主题模式（脏数据/未知值一律回落默认） */
export function asThemeMode(v: unknown): ThemeMode {
  return v === "dark" || v === "light" || v === "system" ? v : THEME_DEFAULT;
}

/** 模式 → 实际主题：自选原样返回；跟随系统查系统深浅色 */
export function resolveTheme(mode: ThemeMode): ResolvedTheme {
  if (mode === "dark" || mode === "light") return mode;
  return SCHEME_QUERY.matches ? "dark" : "light";
}

/** 把解析结果写到根元素，CSS 侧 ：root（暗色默认）/[data-theme="light"] 据此切换。
 *  已知取舍（2026-10-10 审查拍板）：模式存后端 app_settings，异步 get_settings
 *  到达前首帧按 ：root 默认深色渲染——浅色系统用户首帧可能闪深色。根治需
 *  index.html 内联阻塞脚本（system 模式 matchMedia 同步可解、自选模式需
 *  localStorage 镜像配合），岛窗透明感知弱，收益不抵侵入，接受现状 */
function applyTheme(resolved: ResolvedTheme): void {
  document.documentElement.dataset.theme = resolved;
}

/**
 * 主题 Hook：各窗口（岛/设置/报表）入口各调一次。
 * - 挂载时读取已存模式；监听设置页保存后的 theme-changed 广播（含自身窗口）
 * - system 模式下订阅系统深浅色变化，实时跟随
 * - 返回当前实际主题（需要按主题换色的场景用，如 ECharts 配色；纯 CSS 场景忽略返回值）
 * - syncNative（07-UX 2.4）：带装饰的辅助窗口传 true，把主题模式同步给原生
 *   标题栏（dark/light 直接下发；system 传 null 回归系统跟随——标题栏此后由
 *   系统原生跟随，无需逐次追解析结果，故 effect 建在 mode 上足够）。岛与托盘
 *   菜单为无边框透明窗无标题栏，不传保持现状。调用容错（catch 吞掉）：
 *   旧系统若 setTheme 不生效不报错不劣化；权限缺失时被 ACL 拒绝同样静默
 */
export function useTheme(opts?: { syncNative?: boolean }): ResolvedTheme {
  const [mode, setMode] = useState<ThemeMode>(THEME_DEFAULT);
  const [resolved, setResolved] = useState<ResolvedTheme>(() => resolveTheme(THEME_DEFAULT));

  // 模式变化 → 重新解析并落到根元素；system 模式额外订阅系统切换
  useEffect(() => {
    const r = resolveTheme(mode);
    applyTheme(r);
    setResolved(r);
    if (mode !== "system") return;
    const onSchemeChange = () => {
      const nr = resolveTheme("system");
      applyTheme(nr);
      setResolved(nr);
    };
    SCHEME_QUERY.addEventListener("change", onSchemeChange);
    return () => SCHEME_QUERY.removeEventListener("change", onSchemeChange);
  }, [mode]);

  // 原生标题栏同步（07-UX 2.4）：消除「选深色而系统浅色时白标题栏＋深内容
  // 同窗两截」。初始挂载时 mode 还是默认 system（setTheme(null) 幂等无害），
  // 真实模式从 get_settings / theme-changed 到达后按需下发
  useEffect(() => {
    if (!opts?.syncNative) return;
    getCurrentWebviewWindow()
      .setTheme(mode === "system" ? null : mode)
      .catch(() => {});
  }, [mode, opts?.syncNative]);

  // 启动读已存模式 + 监听设置页广播（emit 广播全部窗口，本窗口也能收到）
  useEffect(() => {
    let alive = true;
    invoke<Record<string, string>>("get_settings")
      .then((s) => {
        if (alive) setMode(asThemeMode(s[THEME_KEY]));
      })
      .catch(() => {});
    const un = listen<ThemeMode>("theme-changed", (e) => setMode(asThemeMode(e.payload)));
    return () => {
      alive = false;
      un.then((f) => f());
    };
  }, []);

  return resolved;
}
