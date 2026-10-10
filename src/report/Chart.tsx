/**
 * ECharts React 薄封装（01-RESEARCH §9 社区标准配方）：
 * 挂载时 init → 卸载时 dispose（不销毁会泄漏 zrender 实例）→
 * ResizeObserver 监听容器尺寸变化触发 resize → option 变更时 setOption
 */
import { useEffect, useRef } from "react";
import * as echarts from "echarts/core";
import type { EChartsCoreOption } from "echarts/core";

export default function Chart({
  option,
  height,
}: {
  option: EChartsCoreOption;
  /** 容器高度：数字（px）或任意 CSS 高度串。五轮审查起报表页传
   *  clamp(min(vh,vw)) 响应式串——宽度与高度双维自适应（ResizeObserver
   *  对容器两向变化都会触发 resize，echarts 按计算后的高度初始化） */
  height: number | string;
}) {
  const ref = useRef<HTMLDivElement>(null);

  // 生命周期：仅挂载/卸载各执行一次
  useEffect(() => {
    const el = ref.current!;
    const chart = echarts.init(el);
    const onResize = () => chart.resize();
    const ro = new ResizeObserver(onResize);
    ro.observe(el);
    return () => {
      ro.disconnect();
      chart.dispose();
    };
  }, []);

  // 数据/配置更新：复用已有实例，不重建。
  // replaceMerge:["series"]（2026-10-10 审查修复）：默认 merge 模式下，新 option
  // 的 series 数量少于上一次时（趋势图 Token 4 系列→单指标 1 系列、按 Agent N
  // 系列→四分类、额度曲线停用实例后刷新），多余旧系列不会消失而是带旧数据继续
  // 叠加渲染（图例残留补丁只藏得住图例藏不住系列本体）；replaceMerge 让 series
  // 整体置换，其余组件（图例交互状态/坐标轴等）仍走 merge 不受影响
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    echarts.getInstanceByDom(el)?.setOption(option, { replaceMerge: ["series"] });
  }, [option]);

  return <div ref={ref} style={{ width: "100%", height }} />;
}
