/**
 * 数字补间 hook（2026-09-28 视觉改版批次一）：
 * 快照 10s 一刷，额度百分比/今日 token 等数值跳变生硬——300ms ease-out 补间
 * 让数字「滚」到新值，仪表质感的关键小件。系统开启「减弱动态效果」时
 * 直接落目标值（与 App.css 全局 reduced-motion 收敛同一纪律）。
 */
import { useEffect, useRef, useState } from "react";

/** 减弱动态效果查询（模块级常量：媒体特性运行期不变，无需重建） */
const REDUCED = window.matchMedia("(prefers-reduced-motion: reduce)");

/** 数值补间：返回「当前应显示的值」，目标变化时 300ms 内从现值滑到目标 */
export function useTween(target: number, durationMs = 300): number {
  const [shown, setShown] = useState(target);
  // 现值存 ref：rAF 逐帧推进不经过 React 状态，避免每帧重挂 effect
  const shownRef = useRef(target);
  const rafRef = useRef(0);

  useEffect(() => {
    cancelAnimationFrame(rafRef.current);
    const from = shownRef.current;
    const diff = target - from;
    // 无差异/差异极小/系统减动效：直接落点，不空转 rAF
    if (Math.abs(diff) < 0.5 || REDUCED.matches) {
      shownRef.current = target;
      setShown(target);
      return;
    }
    const t0 = performance.now();
    const tick = (t: number) => {
      const p = Math.min(1, (t - t0) / durationMs);
      // ease-out cubic：先快后慢，与全局 --ease-out 气质一致
      const eased = 1 - Math.pow(1 - p, 3);
      const v = from + diff * eased;
      shownRef.current = v;
      setShown(v);
      if (p < 1) rafRef.current = requestAnimationFrame(tick);
    };
    rafRef.current = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(rafRef.current);
  }, [target, durationMs]);

  return shown;
}
