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
