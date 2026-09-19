/**
 * Agent 自定义颜色注入 Hook（M1-11 补窗口差异）：
 * 岛窗口启动时读取设置项 agent_colors 并订阅设置页 agents-changed 推送，
 * 但会话/报表窗口此前没做这一步——自定义颜色不生效（用户实测踩中）。
 * 本 Hook 供常规窗口挂载时调用：读一次设置注入全局色表（types.setAgentColors），
 * 并实时跟随设置变更；返回颜色表本身，变更时驱动本组件重渲染
 */
import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { setAgentColors } from "./types";

export function useAgentColors(): Record<string, string> {
  const [colors, setColors] = useState<Record<string, string>>({});

  useEffect(() => {
    let alive = true;
    // 读取一次设置并注入；成功返回 true。窗口极早期加载时挂载瞬间的 invoke
    // 可能被无声丢弃且 promise 永不落定（岛窗口 2026-09-17 实测教训），
    // 故立即尝试 + 每 1s 重试直至成功，成功后停表
    let timer: number | undefined;
    const attempt = async (): Promise<void> => {
      if (done || !alive) return;
      try {
        const s = await invoke<Record<string, string>>("get_settings");
        if (!alive) return;
        if (s.agent_colors) {
          try {
            const c = JSON.parse(s.agent_colors) as Record<string, string>;
            if (c && typeof c === "object") {
              setAgentColors(c);
              setColors(c);
            }
          } catch {
            /* 解析失败用默认色表 */
          }
        }
        done = true;
        if (timer != null) window.clearInterval(timer);
      } catch {
        /* 通道未就绪，下一轮重试 */
      }
    };
    let done = false;
    timer = window.setInterval(() => void attempt(), 1000);
    void attempt();
    // 实时跟随设置页变更（设置页保存颜色后广播；推送发生在用户操作时，无需重试）
    const un = listen<{ agents: string[]; colors: Record<string, string> }>(
      "agents-changed",
      (e) => {
        setAgentColors(e.payload.colors);
        setColors(e.payload.colors);
      },
    );
    return () => {
      alive = false;
      if (timer != null) window.clearInterval(timer);
      un.then((f) => f());
    };
  }, []);

  return colors;
}
