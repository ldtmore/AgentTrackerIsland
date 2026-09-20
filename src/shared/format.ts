/**
 * 显示格式化小工具（M1-10 从报表页抽出共享）：
 * 报表页与会话窗口共用，避免同一段格式化逻辑两处漂移
 */

/** 路径取尾段（F:\repo\projA → projA；显示用，过滤值仍用全路径） */
export function tail(p: string): string {
  return p.split(/[\\/]/).filter(Boolean).pop() ?? p;
}

/** 毫秒 → "MM-dd HH:mm"（本机时区） */
export function fmtDT(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** 毫秒 → 完整 "YYYY-MM-dd HH:mm:ss"（悬浮提示用） */
export function fmtDTFull(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** 毫秒 → "900 毫秒"/"1.2 秒"（平均首字延迟用） */
export function fmtMs(ms: number): string {
  return ms < 1000 ? `${ms} 毫秒` : `${(ms / 1000).toFixed(1)} 秒`;
}

/** 毫秒 → "HH:mm:ss"（抽屉流水行内时间，秒级区分同分钟内的先后；日期由分组分隔行承载） */
export function fmtHMS(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** 星期中文短标签（daySepLabel 用） */
const WEEKDAYS = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];

/** 同一自然日判断（年月日全等，跨年也正确） */
function isSameDay(a: Date, b: Date): boolean {
  return (
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate()
  );
}

/** 毫秒 → 流水日期分组标签：今天／昨天／"MM-dd（周X）"（抽屉流水按自然日分段用） */
export function daySepLabel(ms: number): string {
  const d = new Date(ms);
  const now = new Date();
  if (isSameDay(d, now)) return "今天";
  if (isSameDay(d, new Date(now.getTime() - 86_400_000))) return "昨天";
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())}（${WEEKDAYS[d.getDay()]}）`;
}
