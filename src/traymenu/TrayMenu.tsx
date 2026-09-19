/**
 * 托盘菜单页（#tray-menu）：webview 自绘托盘菜单（M1-9）。
 * 原生菜单（muda → Win32）无样式 API，托盘右键改为弹出本窗口：
 * 行高/间距/圆角/动效/双主题全部可控，样式只引用 App.css 的岛面板变量体系。
 * 数据零新增查询——信息头订阅与岛窗口同一份 island-snapshot 广播（聚合器 10s 一拍）；
 * 动作经 tray_menu_action 复用托盘旧有逻辑（toggle_island/show_aux_window/退出）。
 * 收起路径：失焦（Rust 侧 Focused 事件）、Esc、点击菜单项后自隐。
 */
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { listen } from "@tauri-apps/api/event";
import { useTheme } from "../shared/theme";
import {
  fmtCountdownCN,
  fmtTokens,
  quotaLevel,
  SESSION_META,
  tensestQuota,
  windowLabel,
  type IslandSnapshot,
  type SessionState,
  type Thresholds,
} from "../shared/types";
import { GearIcon, InfoIcon, IslandIcon, PowerIcon, ReportIcon, SessionsIcon, WarnIcon } from "../shared/icons";
import "./traymenu.css";

/** 菜单窗口宽度（与 tauri.conf.json tray-menu.width 保持一致；高度自适应内容，无需同步） */
const MENU_WIDTH = 280;

/** 头部状态行的展示顺序（严重度优先：出错 > 等待输入 > 工作中）。
 *  空闲/离线不上桌——「没有事发生」的会话不占头部空间，总数在全空闲时交代一次；
 *  文案复用岛面板的 SESSION_META，与岛同一套词，零图例成本 */
const STATE_ORDER: SessionState[] = ["error", "waiting", "working"];

/** 单行菜单项：左图标 + 文案 + 右侧徽标（可选）；退出项红色悬停（danger） */
function Item({
  icon,
  label,
  badge,
  danger,
  onClick,
}: {
  icon: React.ReactNode;
  label: string;
  badge?: string;
  danger?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      className={`tm-item${danger ? " tm-item-danger" : ""}`}
      onClick={onClick}
    >
      <span className="tm-item-ico">{icon}</span>
      <span className="tm-item-label">{label}</span>
      {badge != null && <span className="tm-item-badge">{badge}</span>}
    </button>
  );
}

export default function TrayMenu() {
  useTheme(); // 深浅主题跟随（与其他窗口同一套 Hook）
  const [snap, setSnap] = useState<IslandSnapshot | null>(null);
  const [islandVisible, setIslandVisible] = useState(true);
  // 每次托盘弹出自增：作为 key 重挂整棵树，重放 CSS 入场动画
  const [openSeq, setOpenSeq] = useState(0);
  // 额度警示阈值（与岛同源：设置页 threshold_warn/threshold_danger，缺省 80/95）
  const [thresholds, setThresholds] = useState<Thresholds>({ warn: 80, danger: 95 });
  // 高度自适应的测量目标（根元素高度由内容撑开，见 traymenu.css）
  const rootRef = useRef<HTMLDivElement>(null);

  const hideMenu = () => {
    getCurrentWindow().hide().catch(() => {});
  };

  useEffect(() => {
    // 快照与岛窗口同源：菜单开着时数据也随 10s 节拍保持新鲜
    const unSnap = listen<IslandSnapshot>("island-snapshot", (e) => setSnap(e.payload));
    // 每次托盘弹出前，Rust 推送最新的岛可见性（比本页内存态可靠）并触发入场动画
    const unShow = listen<{ island_visible: boolean }>("tray-menu-show", (e) => {
      setIslandVisible(e.payload.island_visible);
      setOpenSeq((n) => n + 1);
    });
    return () => {
      unSnap.then((f) => f());
      unShow.then((f) => f());
    };
  }, []);

  // 窗口高度自适应内容（M1-9 修复）：测量根元素实际高度（含 8px 透明边距）后
  // 以逻辑尺寸回写窗口——以后菜单项增减不必再回头改 tauri.conf.json 的高度。
  // 根元素高度由内容撑开（不锁 100vh），窗口尺寸变化不会反向触发观察器，无回环；
  // openSeq 变化时根元素随 key 重挂，故依赖它重挂观察器。conf 里的 height 仅是
  // 首帧兜底，挂载后即被测量值覆盖
  useLayoutEffect(() => {
    const el = rootRef.current;
    if (!el) return;
    const apply = () => {
      const h = el.offsetHeight;
      if (h > 0) getCurrentWindow().setSize(new LogicalSize(MENU_WIDTH, h)).catch(() => {});
    };
    apply();
    const ro = new ResizeObserver(apply);
    ro.observe(el);
    return () => ro.disconnect();
  }, [openSeq]);

  // 阈值挂载时读一次设置（额度行变色用，与岛面板同一套；读不到用缺省，不阻塞渲染）
  useEffect(() => {
    invoke<Record<string, string>>("get_settings")
      .then((s) => {
        const warn = Number(s.threshold_warn);
        const danger = Number(s.threshold_danger);
        setThresholds({
          warn: Number.isFinite(warn) && warn > 0 ? warn : 80,
          danger: Number.isFinite(danger) && danger > 0 ? danger : 95,
        });
      })
      .catch(() => {});
  }, []);

  // Esc 收起（失焦收起在 Rust 侧窗口事件，不依赖本页 JS 存活）
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") hideMenu();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // 动作分发后自隐：先 invoke 再 hide，避免点击反馈丢失；隐藏是幂等操作
  const act = (action: string) => {
    invoke("tray_menu_action", { action }).catch(() => {});
    hideMenu();
  };

  // 点击面板外的窗口空白（四周 8px 圆角边距）也收起——该区域属于本窗口，
  // 不会触发 Rust 侧失焦事件，需自行兜底
  const onRootClick = (e: React.MouseEvent<HTMLDivElement>) => {
    if (e.target === e.currentTarget) hideMenu();
  };

  // 头部三行各说一件事，零重复：①状态分布（只列非零「有事」状态，按严重度排序）；
  // ②今日消耗（渲染处）；③最紧张额度窗口
  const stateCounts = new Map<SessionState, number>();
  for (const s of snap?.sessions ?? []) {
    stateCounts.set(s.state, (stateCounts.get(s.state) ?? 0) + 1);
  }
  const busyChips = STATE_ORDER.map((state) => ({
    state,
    label: SESSION_META[state].label,
    count: stateCounts.get(state) ?? 0,
  })).filter((c) => c.count > 0);

  // ③额度行数据：选择器与岛胶囊同一套（tensestQuota：百分比最高优先，并列取更短窗口）。
  // GLM 未配置或无额度数据时整行隐藏不占空间；额度耗尽时红字提示
  const tense = snap ? tensestQuota(snap.quotas) : null;
  const quotaWindow = snap?.glm_configured && !snap.quota_exhausted ? tense : null;
  const quotaPct =
    quotaWindow?.used_percent != null
      ? Math.round(Math.min(100, Math.max(0, quotaWindow.used_percent)))
      : null;
  const quotaLevelCls =
    quotaPct != null ? quotaLevel(quotaPct, thresholds.warn, thresholds.danger) : "normal";

  return (
    <div className="tm-root" key={openSeq} ref={rootRef} onClick={onRootClick}>
      <div className="tm-panel">
        {/* 信息头：三行各说一件事——状态分布 / 今日消耗 / 最紧张额度，零重复；
            degraded 时诚实降级提示 */}
        <div className="tm-head">
          <div className="tm-head-line1">
            {busyChips.length > 0 ? (
              <div className="tm-head-states">
                {busyChips.map((c) => (
                  <span key={c.state} className={`tm-chip chip-${c.state}`}>
                    <i className="tm-dot" />
                    <b>{c.count}</b>
                    {c.label}
                  </span>
                ))}
              </div>
            ) : (
              <span className="tm-head-quiet">
                {snap
                  ? snap.sessions.length > 0
                    ? `${snap.sessions.length} 个会话全部空闲`
                    : "未采集到会话"
                  : "启动中…"}
              </span>
            )}
            {snap?.degraded && (
              <span className="tm-head-warn">
                <WarnIcon />
              </span>
            )}
          </div>
          <div className={`tm-head-line2${snap?.degraded ? " tm-head-warn-text" : ""}`}>
            {snap?.degraded
              ? "数据降级中，统计可能不完整"
              : snap && snap.today_calls > 0
                ? `今日 ${fmtTokens(snap.today_tokens)} tokens · ${snap.today_calls} 次调用`
                : "今日暂无用量"}
          </div>
          {(quotaWindow != null || snap?.quota_exhausted) && (
            <div className="tm-head-line2 tm-head-quota">
              {quotaWindow == null ? (
                <span className="tm-quota-exhausted">额度已用尽，等待窗口重置</span>
              ) : (
                <>
                  额度 <b className={`text-${quotaLevelCls}`}>{quotaPct}%</b>
                  <span className="tm-quota-sub">
                    （{windowLabel(quotaWindow.window_kind)}）
                    {quotaWindow.reset_at != null && (
                      <> · {fmtCountdownCN(quotaWindow.reset_at)}后重置</>
                    )}
                  </span>
                </>
              )}
            </div>
          )}
        </div>
        <div className="tm-sep" />
        <Item
          icon={<IslandIcon />}
          label={islandVisible ? "隐藏灵动岛" : "显示灵动岛"}
          badge={islandVisible ? "可见" : "已隐藏"}
          onClick={() => act("toggle")}
        />
        <Item icon={<ReportIcon />} label="报表" onClick={() => act("report")} />
        <Item icon={<SessionsIcon />} label="会话" onClick={() => act("sessions")} />
        <Item icon={<GearIcon />} label="设置" onClick={() => act("settings")} />
        <Item icon={<InfoIcon />} label="关于" onClick={() => act("about")} />
        <div className="tm-sep" />
        <Item icon={<PowerIcon />} label="退出" danger onClick={() => act("quit")} />
      </div>
    </div>
  );
}
