/**
 * 岛悬停气泡窗口（第七轮审查 4.1 品牌化重做）：静态常驻第三个窗口（label
 * "tooltip"），替代岛胶囊的原生系统 title——白底黑字系统气泡与全应用深色
 * 玻璃语言割裂、只能纯文本排版、出现要等约 1 秒；本窗口渲染结构化品牌气泡
 * （复用 .tip 玻璃配方），深浅主题随变量。
 *
 * 协议（岛窗口 → 本窗口）：emit_to("tooltip", "island-tooltip", payload)
 *   - { action: "show", data: TooltipPayload }（结构化渲染数据，shared/islandTooltip）
 *   - { action: "hide" }
 *
 * 显示时序：show 到达 → 渲染气泡（visibility:hidden 不闪默认位）→
 * useLayoutEffect 量气泡真实尺寸 → invoke island_tooltip_place（Rust 按
 * 光标定位＋按尺寸定窗＋show）→ 返回后才挂 ttw-in 入场动画（淡入＋微移）。
 * hide 到达 → 清内容＋invoke island_tooltip_hide（幂等）。
 *
 * ⚠ 边界约定：气泡恒非交互（Rust 创建后立即 set_ignore_cursor_events(true)
 * 恒穿透），本窗口不消费任何指针事件；内容锚定快照不热更新（岛端 hide 后
 * 下次悬停重新 emit，避免悬停读数时内容跳变）。
 */
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useTheme } from "../shared/theme";
import type { TooltipPayload, TooltipLine } from "../shared/islandTooltip";

/** 单条数据行渲染：非 normal 档前置 ⚠（纯文本版同款语义——行内唯一强调手段），
 *  数字段 mono＋档位色，附注段弱色 */
function Line({ line }: { line: TooltipLine }) {
  return (
    <div className="ttw-row">
      {line.level !== "normal" && <span className="ttw-flag">⚠ </span>}
      {line.label && <span className="ttw-dim">{line.label} </span>}
      <span className={`ttw-num ttw-${line.level}`}>{line.mono}</span>
      {line.post && <span className="ttw-dim"> {line.post}</span>}
    </div>
  );
}

export default function TooltipWindow() {
  // 主题应用与跟随（M1-4）：写 <html> data-theme，CSS 变量自动切换（不传
  // syncNative——本窗口无边框透明无标题栏）
  useTheme();
  const [data, setData] = useState<TooltipPayload | null>(null);
  // place 完成后才挂入场动画类（窗口未 show 前动画已播＝看不到入场，见头注时序）
  const [placed, setPlaced] = useState(false);
  const boxRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const un = listen<
      { action: "show"; data: TooltipPayload } | { action: "hide" }
    >("island-tooltip", (e) => {
      if (e.payload.action === "show") {
        setPlaced(false);
        setData(e.payload.data);
      } else {
        setData(null);
        invoke("island_tooltip_hide").catch(() => {});
      }
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  // 气泡已渲染（窗口仍隐藏）→ 量真实尺寸 → Rust 定位＋定窗＋show → 播入场动画。
  // place 失败也置 placed（窗口按默认位 show，降级可见优于无响应）
  useLayoutEffect(() => {
    if (!data) return;
    const el = boxRef.current;
    if (!el) return;
    const b = el.getBoundingClientRect();
    invoke("island_tooltip_place", { w: b.width, h: b.height })
      .catch(() => {})
      .then(() => setPlaced(true));
  }, [data]);

  if (!data) return null;
  return (
    <div
      ref={boxRef}
      className={`tip ttw${placed ? " ttw-in" : ""}`}
      role="tooltip"
    >
      <div className="ttw-status">{data.status}</div>
      {data.degraded && (
        <div className="ttw-row">
          <span className="ttw-flag">⚠ </span>
          <span className="ttw-danger">采集源连续失败，数据可能滞后</span>
        </div>
      )}
      {data.today && <div className="ttw-row">{data.today}</div>}
      {data.allTime && <div className="ttw-row ttw-dim">{data.allTime}</div>}
      {data.quotaNote && <div className="ttw-row ttw-dim">{data.quotaNote}</div>}
      {data.groups.map((g) => (
        <div className="ttw-group" key={g.title}>
          <div className="ttw-group-title">
            <i className="ttw-dot" style={{ background: g.color }} />
            {g.title}
          </div>
          {g.balance && <Line line={g.balance} />}
          {g.rows.map((r) => (
            <Line key={r.mono} line={r} />
          ))}
          {g.empty && <div className="ttw-row ttw-dim">{g.empty}</div>}
        </div>
      ))}
    </div>
  );
}
