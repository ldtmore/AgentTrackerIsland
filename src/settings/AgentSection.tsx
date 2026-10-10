/**
 * 「Agent 监控」分区（2026-10-03 四轮审查拆分自 Settings.tsx，JSX/行为不变）：
 * Agent 勾选、身份色自定义（撞色校验）、hooks 注入/卸载；
 * 2026-10-03 起含卡片拖拽排序（agents_order 键，全应用展示序真值）。
 * 拆分动机：6 个状态（agents/agentColors/savedColorsRef/colorError/hooksOn/hookBusy）
 * 只服务本分区——拖色/勾选的重渲染收敛在分区内，不再全页 reconcile。
 * 自加载：挂载时读 agents_enabled/agent_colors 并逐家查 hooks 状态。
 */
import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import { AGENT_COLORS, AGENT_DEFS, HOOKS_AGENTS } from "../shared/types";
import {
  AGENT_LIMIT_MAX,
  parseAgentLimit,
  parseAgentOrder,
} from "../shared/agentOrder";
import { SortableItem, SortableList, moveIn } from "../shared/Sortable";
import AgentBadge from "../shared/AgentBadge";
import { invokeReady } from "../shared/invokeReady";
import { EyeIcon, Row, Section, Stepper, Switch } from "./widgets";

/** AGENT_DEFS 元素类型（order 重排 map/find 的类型收窄用） */
type AgentDef = (typeof AGENT_DEFS)[number];

export default function AgentSection({
  saveKey,
  showToast,
}: {
  /** 单键落库（失败 toast），由编排层提供保持单一来源 */
  saveKey: (key: string, value: string) => Promise<boolean>;
  showToast: (text: string, kind: "ok" | "error") => void;
}) {
  // 监控的 Agent 列表（缺省全选；勾选才采集/监控/展示）
  const [agents, setAgents] = useState<string[]>(AGENT_DEFS.map((a) => a.id));
  // 卡片展示顺序（设置键 agents_order；缺省＝AGENT_DEFS 声明序。全应用
  // 的 Agent 枚举位——面板筛选菜单/贴边分段条/报表图例、后端报表下拉——
  // 均以此序展示，2026-10-03 拖拽排序）
  const [order, setOrder] = useState<string[]>(AGENT_DEFS.map((a) => a.id));
  // 上岛深度（设置键 island_agent_limit，2026-10-08）：拖拽序前 N 个 Agent
  // 上岛（贴边隐藏态分段只渲染这些），其余照常采集不上岛；默认 5、上限 6
  const [limit, setLimit] = useState<number>(parseAgentLimit(undefined));
  // Agent 自定义身份色（未自定义的用系统默认色；隐藏态色块/面板徽标共用）
  const [agentColors, setAgentColors] = useState<Record<string, string>>({});
  // 已落库的自定义色：拖拽选色过程中实时改 UI 但不落库，失焦校验撞色后按此回滚
  const savedColorsRef = useRef<Record<string, string>>({});
  const [colorError, setColorError] = useState("");
  // hooks 安装状态（M2-6/7 多 Agent：agent id → 是否已注入；三家各自独立）
  const [hooksOn, setHooksOn] = useState<Record<string, boolean>>({});
  const [hookBusy, setHookBusy] = useState<Record<string, boolean>>({}); // 各家注入/卸载进行中，防连点

  // 自加载本分区设置切片（与编排层各自读 get_settings：本地毫秒级，省去逐项下传）
  useEffect(() => {
    (async () => {
      try {
        const s = await invokeReady<Record<string, string>>("get_settings");
        if (s.agents_enabled) {
          try {
            const list = JSON.parse(s.agents_enabled) as string[];
            if (Array.isArray(list)) setAgents(list);
          } catch {
            /* 解析失败用默认全选 */
          }
        }
        if (s.agents_order) {
          setOrder(parseAgentOrder(s.agents_order));
        }
        // 上岛深度（键缺失/损坏由 parseAgentLimit 归一默认 5）
        setLimit(parseAgentLimit(s.island_agent_limit));
        if (s.agent_colors) {
          try {
            const colors = JSON.parse(s.agent_colors) as Record<string, string>;
            if (colors && typeof colors === "object") {
              setAgentColors(colors);
              savedColorsRef.current = colors;
            }
          } catch {
            /* 解析失败用默认色 */
          }
        }
      } catch {
        /* 加载失败保持默认值 */
      }
      await refreshHooksStatus();
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** 已勾选 Agent 的展示色（自定义 → 系统默认） */
  const effColor = (id: string) => agentColors[id] ?? AGENT_COLORS[id];

  /** 校验已勾选 Agent 间颜色不重复（颜色 = 身份）；通过则落库并推送岛窗口，撞色返回 false。
   *  落库失败同样返回 false 且不广播（五轮审查批次一）：岛端维持已保存的旧色，
   *  调用方按 savedColorsRef 回滚 UI——「假生效」与 changeTheme/saveKey 门控同纪律 */
  const commitColors = async (colors: Record<string, string>, list: string[]) => {
    const seen = new Map<string, string>();
    for (const a of AGENT_DEFS.filter((x) => list.includes(x.id))) {
      const c = (colors[a.id] ?? AGENT_COLORS[a.id]).toLowerCase();
      if (seen.has(c)) {
        setColorError(`「${seen.get(c)}」与「${a.label}」颜色相同，请改用不同颜色`);
        return false;
      }
      seen.set(c, a.label);
    }
    setColorError("");
    if (!(await saveKey("agent_colors", JSON.stringify(colors)))) return false;
    savedColorsRef.current = colors; // 仅成功后推进（失败时保持上次已保存值供回滚）
    await emit("agents-changed", { agents: list, colors }).catch(() => {});
    return true;
  };

  /** 勾选/取消 Agent：即存即生效；勾选集变化会改变撞色判定范围，顺带重新校验。
   *  保存失败回滚勾选并跳过后续（五轮审查批次一） */
  const toggleAgent = async (id: string, checked: boolean) => {
    const prev = agents;
    const next = checked ? [...agents, id] : agents.filter((x) => x !== id);
    setAgents(next);
    if (!(await saveKey("agents_enabled", JSON.stringify(next)))) {
      setAgents(prev);
      return;
    }
    await commitColors(agentColors, next);
  };

  /** 拖拽重排（2026-10-03）：即存即生效（agents_order 键），失败回滚——与
   *  勾选同纪律。order 变化经 agents-changed 广播岛端（载荷新增 order 字段；
   *  勾选/改色两条既有 emit 不带 order，岛端按可选字段处理、维持原序） */
  const applyOrder = async (next: string[]) => {
    const prev = order;
    setOrder(next);
    if (!(await saveKey("agents_order", JSON.stringify(next)))) {
      setOrder(prev);
      return;
    }
    await emit("agents-changed", { agents, colors: agentColors, order: next }).catch(() => {});
  };

  /** 上岛深度调整（2026-10-08）：即存即生效（island_agent_limit 键），失败回滚
   *  ——与拖拽/勾选同纪律。落库成功后 agents-changed 事件带 limit 字段广播
   *  岛端（岛窗口更新注册表，贴边分段下一帧收敛）；设置页自身不消费深度 */
  const applyLimit = async (next: number) => {
    const prev = limit;
    setLimit(next);
    if (!(await saveKey("island_agent_limit", String(next)))) {
      setLimit(prev);
      return;
    }
    await emit("agents-changed", { agents, colors: agentColors, order, limit: next }).catch(() => {});
  };

  /** 颜色选择失焦 = 改完：撞色回滚本次改动并就近提示，通过则落库 */
  const onColorBlur = () => {
    commitColors(agentColors, agents).then((ok) => {
      if (!ok) setAgentColors({ ...savedColorsRef.current });
    });
  };

  /** 恢复全部 Agent 默认身份色（失败回滚到上次已保存色，不弹「已恢复」） */
  const resetColors = async () => {
    setAgentColors({});
    const ok = await commitColors({}, agents);
    if (!ok) {
      setAgentColors({ ...savedColorsRef.current });
      return;
    }
    showToast("已恢复默认颜色", "ok");
  };

  /** hooks 注入/卸载（M2-6/7 起按 Agent 独立装卸）：本地文件操作，busy 态记录到各家防连点。
   *  成功后广播 hooks-changed（07-UX 1.1 配套）：hooks 装卸不改任何设置键也无既有事件，
   *  引导卡的「精确实时状态」步依赖它实时打勾；无订阅方时静默无害 */
  const toggleHooks = async (agent: string) => {
    setHookBusy((prev) => ({ ...prev, [agent]: true }));
    try {
      if (hooksOn[agent]) await invoke("uninstall_hooks", { agent });
      else await invoke("install_hooks", { agent });
      const on = await invoke<boolean>("hooks_status", { agent });
      setHooksOn((prev) => ({ ...prev, [agent]: on }));
      await emit("hooks-changed", agent).catch(() => {});
      showToast(
        on ? "已注入 hooks：实时精确状态已启用" : "已卸载 hooks：回到启发式状态",
        "ok",
      );
    } catch (e) {
      showToast(`操作失败：${e}`, "error");
    } finally {
      setHookBusy((prev) => ({ ...prev, [agent]: false }));
    }
  };

  /** 逐家查询 hooks 安装状态（并行；单家失败不影响其他家显示） */
  const refreshHooksStatus = async () => {
    const results = await Promise.all(
      HOOKS_AGENTS.map(async (agent) => {
        try {
          return [agent, await invoke<boolean>("hooks_status", { agent })] as const;
        } catch {
          return [agent, false] as const;
        }
      }),
    );
    setHooksOn(Object.fromEntries(results));
  };

  // 展示序＝用户拖拽序（order 恒为完整置换，find 不会落空；防御性 filter 兜底）
  const orderedDefs = order
    .map((id) => AGENT_DEFS.find((a) => a.id === id))
    .filter((a): a is AgentDef => a !== undefined);

  return (
    <Section title="Agent 监控" id="st-section-agents" desc="管理各 Agent 的监控、精确状态与身份颜色；拖动卡片可调整全应用的展示顺序、前 N 个上岛">
      {/* 上岛深度（2026-10-08）：控件在前、结果（卡片）在后——与报表页筛选动线
          同哲学，调 N 时下方卡片即时呈现岛内/岛外分界 */}
      <Row
        title="在灵动岛展示前 N 个"
        desc="按拖拽顺序，仅前 N 个 Agent 出现在灵动岛的贴边隐藏态中"
        tip="灵动岛最多展示 6 个；其余 Agent 仍正常监控，报表/会话数据不受影响；界外 Agent 出错时岛仍会以红边与文案提示，详情进面板查看"
      >
        <Stepper
          value={limit}
          min={1}
          max={AGENT_LIMIT_MAX}
          ariaLabel="在灵动岛展示的 Agent 数量"
          onChange={(v) => void applyLimit(v)}
        />
      </Row>
      {/* 卡片网格（2026-09-23 改版）：列数随窗口宽度自适应（L1 限宽后 3 列封顶）；
          2026-10-03 起按用户拖拽序渲染（agents_order），SortableItem 包裹卡片段
          使 grip 手柄成为网格单元（卡片原有点击/取色/开关不受手柄影响） */}
      <SortableList
        ids={order}
        onDragEnd={(e) => {
          if (!e.over) return;
          const next = moveIn(order, String(e.active.id), String(e.over.id));
          if (next) void applyOrder(next);
        }}
      >
        <div className="st-agents">
          {orderedDefs.map((a, i) => {
            const checked = agents.includes(a.id);
            // 岛外卡（2026-10-08）：拖拽序在 N 之外＝照常采集但不上岛；
            // 静态序号判定（不剔除未启用）——卡面身份稳定，启停不引发岛内外漂移
            const offIsland = i >= limit;
            // 精确状态开关（M2-6/7 行级整合延续）：仅支持 hooks 的三家有；
            // 未启用（不监控谈不上精确）或装卸进行中时禁用
            const hasHooks = HOOKS_AGENTS.includes(a.id);
            const hooksOnFor = hooksOn[a.id] ?? false;
            const hooksBusyFor = hookBusy[a.id] ?? false;
            // 副标题=当前真实采集方式（文案描述现状而非装饰：已注入/启发式精度/未启用）
            const status = !checked
              ? "未启用"
              : hasHooks && hooksOnFor
                ? "精确状态·hooks 已注入"
                : hasHooks
                  ? "未注入 hooks·约 90 秒精度"
                  : "进程与文件启发式";
            return (
              <SortableItem key={a.id} id={a.id}>
                <div
                  className={`st-agent${checked ? " st-agent-on" : ""}${offIsland ? " st-agent-offisland" : ""}`}
                  onClick={() => toggleAgent(a.id, !checked)}
                  onKeyDown={(e) => {
                    // 焦点在内嵌控件（取色 input/开关）时放行其自身激活，
                    // 不再冒泡执行外层启停（五轮审查批次二：原先 Enter 会把
                    // 「想开精确状态」变成「关掉整个 Agent 监控」的高破坏性错位）
                    if (e.target !== e.currentTarget) return;
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      toggleAgent(a.id, !checked);
                    }
                  }}
                  role="button"
                  tabIndex={0}
                  aria-pressed={checked}
                  title={checked ? "点击停用该 Agent 的监控" : "点击启用该 Agent 的监控"}
                >
                  <div className="st-agent-name">
                    {/* 身份徽章＝名字＝取色入口（2026-09-30 徽章统一改版）：
                        全名胶囊即身份，原生 color input 隐藏为点击代理；
                        拖动取色器经 colorOverride 即时预览（落库前后均跟随） */}
                    <span
                      className="st-agent-badge"
                      title={
                        checked
                          ? "点击自定义该 Agent 在岛上的显示颜色"
                          : "启用后可自定义该 Agent 的显示颜色"
                      }
                      onClick={(e) => e.stopPropagation()}
                    >
                      <AgentBadge
                        agent={a.id}
                        name={a.label}
                        size="lg"
                        gray={!checked}
                        colorOverride={effColor(a.id)}
                      />
                      <input
                        type="color"
                        aria-label={`${a.label} 颜色`}
                        value={effColor(a.id)}
                        disabled={!checked}
                        onChange={(e) =>
                          setAgentColors((prev) => ({ ...prev, [a.id]: e.target.value }))
                        }
                        onBlur={onColorBlur}
                      />
                    </span>
                    <span className="st-agent-switch" onClick={(e) => e.stopPropagation()}>
                      <Switch checked={checked} onChange={(v) => toggleAgent(a.id, v)} />
                    </span>
                  </div>
                  <div className="st-agent-status">
                    {/* 副标题窄卡 ellipsis 截断兜底（C4）：如「未注入 hooks·约 90 秒精度」 */}
                    <span className="st-agent-status-text" title={status}>
                      {status}
                    </span>
                    {/* 岛外小标（2026-10-08）：与未启用的灰显双通道区分——
                        灰显=不采集，退后+小标=照常采集但不上岛。
                        2026-10-09 位置统一：排在「精确」开关之前，恒定紧跟状态
                        文字（左信息右控件）——原先排在其后，是否支持 hooks 决定
                        了它落在行中还是行尾，同一个小标左右漂移（精确开关带
                        margin-left:auto 把后续元素一并推到右缘） */}
                    {offIsland && (
                      <span
                        className="st-agent-offisland-tag"
                        title="未在灵动岛展示（仅拖拽序前 N 个上岛，可在上方调整）"
                      >
                        <EyeIcon off />
                        未上岛
                      </span>
                    )}
                    {hasHooks && (
                      <span
                        className={`st-agent-hooks${checked ? "" : " st-agent-hooks-off"}`}
                        title={
                          checked
                            ? "注入 hooks：Agent 实时上报状态（工作中/等待输入）\n未注入时按进程启发式推断，约 90 秒精度\n装卸均自动备份配置文件"
                            : "先启用该 Agent 监控，再注入 hooks"
                        }
                        onClick={(e) => e.stopPropagation()}
                      >
                        <span className="st-agent-hooks-label">精确</span>
                        <Switch
                          checked={hooksOnFor}
                          disabled={!checked || hooksBusyFor}
                          small
                          onChange={() => toggleHooks(a.id)}
                        />
                      </span>
                    )}
                  </div>
                </div>
              </SortableItem>
            );
          })}
        </div>
      </SortableList>
      {colorError && <div className="st-row-error">{colorError}</div>}
      <div className="st-agent-foot">
        <button type="button" className="st-btn st-btn-sm" onClick={resetColors}>
          恢复默认颜色
        </button>
      </div>
    </Section>
  );
}
