/**
 * 首次引导「三步上手卡」（07-UX 1.1，P0）：新用户首次打开没有任何一处告诉
 * 「接下来做什么」——获得完整体验需自行发现三件事：注入 hooks 才有精确实时
 * 状态（缺省仅约 90 秒启发式精度）、添加供应商才有额度、托盘里有全部入口。
 *
 * 设计（文档定稿口径）：
 * - 一次性：完成态（三步全勾）或点「不再显示」落 app_settings 键
 *   onboarding_done=1，之后不再渲染；本组件自查该键，编排层零逻辑；
 * - 自包含：自拉数据自刷新（get_settings＋hooks_status＋provider_overview），
 *   订阅 agents-changed／provider-accounts-changed／hooks-changed 触发重查，
 *   与 AgentSection／QuotaSection 的自有状态解耦，不向编排层传状态；
 * - 完成判定：① agents_enabled 非空数组（键缺失＝缺省全启用＝满足，
 *   注意第一步不可写成「去勾选」）；② 任一 HOOKS_AGENTS 注入成功；
 *   ③ provider_overview 非空（第三步标注可选）；
 * - 每步带文字链接走 scrollToSection 直达对应分区（键盘可达，真按钮）。
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { HOOKS_AGENTS } from "../shared/types";
import { invokeReady } from "../shared/invokeReady";
import { scrollToSection, QUOTA_SECTION_ID } from "./widgets";

/** 引导卡的三步定义（完成判定见组件内 refresh；section 为滚动直达锚点） */
const STEPS: { title: string; desc: string; section: string; action: string }[] = [
  {
    title: "确认监控范围",
    desc: "已默认监控全部支持的 Agent，可按需调整",
    section: "st-section-agents",
    action: "去调整",
  },
  {
    title: "开启精确实时状态",
    desc: "为支持的 Agent 注入 hooks，状态从约 90 秒精度升级为实时上报",
    section: "st-section-agents",
    action: "去开启",
  },
  {
    title: "添加供应商（可选）",
    desc: "在「额度与凭据」录入 API 凭据，跟踪余额与窗口用量",
    section: QUOTA_SECTION_ID,
    action: "去添加",
  },
];

/** 步骤状态标：完成＝强调色圆圈勾，未完成＝弱色空心圈（线性 SVG，禁字符图标） */
function StepMark({ done }: { done: boolean }) {
  return (
    <svg
      width="14"
      height="14"
      viewBox="0 0 24 24"
      fill="none"
      stroke={done ? "var(--accent)" : "var(--text-4)"}
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <circle cx="12" cy="12" r="9" />
      {done && <path d="M8 12.5l2.8 2.8L16.5 9" />}
    </svg>
  );
}

export default function OnboardingCard() {
  /** null＝引导状态查询中（不渲染防闪烁）；false＝已落键或已完成；
   *  true＝展示中（至少一步未完成） */
  const [visible, setVisible] = useState<boolean | null>(null);
  /** 三步完成态（顺序与 STEPS 一一对应） */
  const [steps, setSteps] = useState<boolean[]>([false, false, false]);
  // 令牌防乱序：事件触发重查与在途查询并发时，后发先至的旧响应不覆盖新状态
  const seq = useRef(0);

  /** 重查三步完成态与一次性键（挂载＋三类事件到达时各调一次）。
   *  get_settings 整体失败（invokeReady 重试后仍失败）保守隐藏——
   *  读不到 onboarding_done 就不能断定「该显示」，误闪比漏显更打扰 */
  const refresh = useCallback(async () => {
    const cur = ++seq.current;
    let s: Record<string, string>;
    try {
      s = await invokeReady<Record<string, string>>("get_settings");
    } catch {
      if (cur === seq.current) setVisible(false);
      return;
    }
    if (cur !== seq.current) return; // 已有更新查询在途，丢弃过期响应
    if (s.onboarding_done === "1") {
      setVisible(false);
      return;
    }
    // ① 监控范围：仅显式空数组＝未完成；键缺失/脏值按缺省全启用＝完成
    let step1 = true;
    if (s.agents_enabled !== undefined) {
      try {
        const list = JSON.parse(s.agents_enabled) as unknown;
        step1 = Array.isArray(list) && list.length > 0;
      } catch {
        /* 解析失败按缺省全启用＝完成 */
      }
    }
    // ② hooks：任一支持注入的 Agent 装上即完成（并行查，单家失败按未注入）
    const hookFlags = await Promise.all(
      HOOKS_AGENTS.map((a) =>
        invoke<boolean>("hooks_status", { agent: a }).catch(() => false),
      ),
    );
    const step2 = hookFlags.some(Boolean);
    // ③ 供应商：实例列表非空即完成（拉取失败按未完成，事件/下次重查会修正）
    let step3 = false;
    try {
      step3 = (await invokeReady<unknown[]>("provider_overview")).length > 0;
    } catch {
      /* 保持未完成 */
    }
    if (cur !== seq.current) return;
    // 三步全勾＝完成态：落一次性键并消失（落键失败无害——下次打开重查再落）
    if (step1 && step2 && step3) {
      setVisible(false);
      void invoke("set_setting", { key: "onboarding_done", value: "1" }).catch(() => {});
      return;
    }
    setSteps([step1, step2, step3]);
    setVisible(true);
  }, []);

  useEffect(() => {
    void refresh();
    // hooks 注入/卸载经 hooks-changed 广播（install_hooks 不改任何设置键，
    // 仅靠 agents-changed 捕捉不到）——「三步逐项完成实时打勾」的验收依据
    const unbind = [
      listen("agents-changed", () => void refresh()),
      listen("provider-accounts-changed", () => void refresh()),
      listen("hooks-changed", () => void refresh()),
    ];
    return () => {
      unbind.forEach((p) => p.then((f) => f()));
    };
  }, [refresh]);

  /** 不再显示：立即隐藏＋落一次性键（落键失败静默——最坏结果是重启后再见一次） */
  const dismiss = () => {
    setVisible(false);
    void invoke("set_setting", { key: "onboarding_done", value: "1" }).catch(() => {});
  };

  if (!visible) return null;
  return (
    <section className="st-onboarding" aria-label="首次使用引导">
      <div className="st-onb-head">
        <div className="st-onb-title">三步上手</div>
        <button type="button" className="st-onb-dismiss" onClick={dismiss}>
          不再显示
        </button>
      </div>
      <div className="st-onb-desc">
        完成以下三步获得完整体验：实时的 Agent 状态、额度跟踪与全部入口（本提示完成后不再出现）
      </div>
      <ul className="st-onb-steps">
        {STEPS.map((st, i) => (
          <li key={st.title} className="st-onb-step">
            <span className="st-onb-mark">
              <StepMark done={steps[i]} />
            </span>
            <span className="st-onb-step-text">
              <span className="st-onb-step-title">{st.title}</span>
              <span className="st-onb-step-desc">{st.desc}</span>
            </span>
            <button
              type="button"
              className="st-onb-link"
              onClick={() => scrollToSection(st.section)}
            >
              {st.action}
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}
