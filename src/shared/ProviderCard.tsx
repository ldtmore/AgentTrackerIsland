/**
 * 供应商实例卡片（M3-4 设置页 / M3-5 额度页共用，06-PLAN §7.1/§8.2）
 * 结构（两层头，2026-09-25 UX 改版）：头行 = 别名 + 岛展示开关 + 启停开关；
 * 次行 = 品牌色点 + 厂商名 + 来源徽标；下接额度摘要 / 凭据异常红字 /
 * base 覆盖留痕 / 检测三态 / 操作行（钉底）。
 * 2026-09-30 身份组分形态：额度页（hideSwitches）头行右侧空置，色点＋厂商名＋
 * 来源徽标上移为右上角角标（每卡省一行）；设置页保持独立次行不变。
 * 文案与交互细则：别名隐去"（自动发现）"后缀由来源徽标承载（悬浮保留全名）；
 * 摘要百分比按阈值水位分色（warn 琥珀/danger 红）；凭据失效时摘要加"历史"
 * 小标并悬浮说明；检测失败文案与凭据异常重复时不再二次渲染。
 * 删除为行内二次点击确认（3 秒复原，无弹窗——红线⑤）
 * M3-5 插槽化：额度页传 detail（替代一行摘要的完整额度体）与 actions
 * （替代检测/编辑/删除的脚注行）即变为信息密度更高的额度卡，并可用
 * hideSwitches 隐藏头行双开关；不传时渲染路径与设置页完全一致
 */
import { useEffect, useRef, useState } from "react";
import type { AccountOverview, ProviderKindView, TestResult } from "./providerTypes";
import { fmtDT, currencySymbol } from "./format";
import { quotaLevel, windowLabel, windowLabelCN, type Thresholds } from "./types";
import "./providercard.css";

/** 取已用比例最高的窗口（最紧张者优先，供摘要与悬浮说明共用） */
function worstWindow(a: AccountOverview) {
  return a.quotas
    .filter((q) => q.used_percent != null)
    .sort((x, y) => (y.used_percent ?? 0) - (x.used_percent ?? 0))[0];
}

/** 额度摘要：Balance 优先（金额大字），Windows 取最紧张窗口。
 *  窗口展示统一短标签 5h/7d（windowLabel），原始值 weekly 不出面向用户；
 *  百分比按阈值水位分色（warn 琥珀/danger 红，阈值缺省 80/95 与后端一致） */
function summaryOf(a: AccountOverview, thresholds?: Thresholds): React.ReactNode {
  if (a.balance) {
    return <>余额 {currencySymbol(a.balance.currency)}{a.balance.total.toFixed(2)}</>;
  }
  const worst = worstWindow(a);
  if (worst) {
    const pct = Math.round(worst.used_percent ?? 0);
    const level = quotaLevel(pct, thresholds?.warn ?? 80, thresholds?.danger ?? 95);
    return (
      <>
        {windowLabel(worst.window_kind)} 已用{" "}
        <span className={level === "normal" ? undefined : `pc-pct-${level}`}>{pct}%</span>
      </>
    );
  }
  return a.enabled ? "暂无额度数据" : "已停用";
}

/** 摘要行的悬浮说明（释义场景用中文全称）：凭据失效时前置历史数据说明 */
function summaryTitleOf(a: AccountOverview): string {
  const time = `最近数据 ${fmtDT(fetchedAtOf(a))}`;
  const stale = a.cred_error ? "凭据失效，展示最近一次成功查询的数据；" : "";
  if (a.balance) return `${stale}余额快照 · ${time}`;
  const worst = worstWindow(a);
  return worst
    ? `${stale}${windowLabelCN(worst.window_kind)}窗口已用比例 · ${time}`
    : `${stale}${time}`;
}

/** 摘要行的数据时间：余额/窗口取快照真实抓取时间（fetched_at）的最大值，
 *  都无则回落实例更新时间。⚠️ 不能用 reset_at（未来的窗口重置时点）参与
 *  计算——那会把"上次刷新"显示成未来时刻（M3-5 额度页审查修正） */
export function fetchedAtOf(a: AccountOverview): number {
  const times = [
    a.balance?.fetched_at ?? 0,
    ...a.quotas.map((q) => q.fetched_at ?? 0),
  ].filter((t) => t > 0);
  return times.length ? Math.max(...times) : a.updated_at;
}

/** 小号滑动开关（岛展示用；启停为主开关用常规尺寸） */
function MiniSwitch({
  on,
  title,
  label,
  onChange,
}: {
  on: boolean;
  title: string;
  label: string;
  onChange: () => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={label}
      title={title}
      className={`pc-switch-sm${on ? " pc-switch-on" : ""}`}
      onClick={onChange}
    >
      <span className="pc-switch-thumb" />
    </button>
  );
}

export default function ProviderCard({
  account,
  kind,
  thresholds,
  testing,
  testResult,
  detail,
  actions,
  hideSwitches,
  onCredAction,
  onToggle,
  onToggleInIsland,
  onTest,
  onEdit,
  onDelete,
}: {
  account: AccountOverview;
  /** 厂商定义（品牌色/显示名；注册表缺该厂商时兜底用 kind_id） */
  kind?: ProviderKindView;
  /** 阈值水位（摘要百分比分色用；缺省 80/95 与后端默认一致） */
  thresholds?: Thresholds;
  /** 检测进行中（转圈态） */
  testing?: boolean;
  /** 最近一次检测结果（✓ 摘要 / ✗ 分类错误） */
  testResult?: TestResult | null;
  /** 额度页插槽：完整额度体（进度条/金额大字），传入时替代一行摘要区 */
  detail?: React.ReactNode;
  /** 额度页插槽：脚注行（时间＋状态徽标＋刷新），传入时替代检测/编辑/删除行 */
  actions?: React.ReactNode;
  /** 额度页无启停管理场景：隐藏头行双开关（纯查看卡） */
  hideSwitches?: boolean;
  /** 凭据红字的动作出口（A3 审查修正）：额度页是纯查看卡没有「编辑」按钮，
   *  红字引导"点「编辑」"会落空——传入时红字行尾渲染「去设置页处理」链接；
   *  设置页不传，红字保持原文（那里编辑按钮就在卡上） */
  onCredAction?: () => void;
  onToggle?: (enabled: boolean) => void;
  onToggleInIsland?: () => void;
  onTest?: () => void;
  onEdit?: () => void;
  /** 二次确认完成后回调（确认交互在卡内） */
  onDelete?: () => void;
}) {
  const color = kind?.color ?? "#8a8f98";
  // 别名隐去"（自动发现）"后缀：来源语义由次行徽标统一承载，避免同卡重复；
  // 悬浮与存储保留完整别名（岛/托盘展示不受影响）
  const displayName = account.alias.replace(/（自动发现）$/, "");
  // 删除二次确认：首点变红进入待确认，3 秒内再点才真删，超时自动复原
  const [confirming, setConfirming] = useState(false);
  const confirmTimer = useRef<number | null>(null);
  useEffect(
    () => () => {
      if (confirmTimer.current !== null) window.clearTimeout(confirmTimer.current);
    },
    [],
  );
  const askDelete = () => {
    if (confirming) {
      setConfirming(false);
      onDelete?.();
      return;
    }
    setConfirming(true);
    if (confirmTimer.current !== null) window.clearTimeout(confirmTimer.current);
    confirmTimer.current = window.setTimeout(() => setConfirming(false), 3000);
  };
  // 检测失败原因与凭据异常一致时不重复渲染（同一段红字出现两遍是视觉噪声）
  const dupWithCredError =
    testResult != null &&
    !testResult.ok &&
    account.cred_error != null &&
    testResult.text === account.cred_error;

  // 身份组（色点＋厂商名＋来源徽标）两形态共用：设置页头行右侧被双开关占用，
  // 独立次行；额度页（hideSwitches 纯查看卡）头行右侧本来空置，上移为右上角
  // 角标——每卡省一整行，厂商降为元信息（2026-09-30 降噪改版）。
  // 厂商名走后端双站变体投影 kind_name（2026-10-10：Z.ai 实例显示 Z.ai 非 GLM）
  const kindName = account.kind_name || kind?.name || account.kind_id;
  const identity = (
    <>
      <i className="pc-dot" style={{ background: color }} />
      <span className="pc-kind">{kindName}</span>
      {account.origin === "discovered" && <span className="pc-origin">自动发现</span>}
    </>
  );

  return (
    <div
      className={`pc-card${account.enabled ? "" : " pc-card-off"}`}
      style={{ "--pc-color": color } as React.CSSProperties}
    >
      {/* 头行：别名 + 右侧身份/开关（单个实例的操作集中一处） */}
      <div className="pc-head">
        <span className="pc-name" title={account.alias}>
          {displayName}
        </span>
        {hideSwitches ? (
          /* 额度页（hideSwitches）为纯查看卡：启停/岛展示管理集中在设置页，
             头行右侧空位放身份组；厂商名窄卡截断，悬浮补全量（含来源） */
          <span
            className="pc-head-id"
            title={`供应商：${kindName}${account.origin === "discovered" ? "（自动发现）" : ""}`}
          >
            {identity}
          </span>
        ) : (
          /* 设置页形态：右侧双开关（岛展示 / 启停） */
          <span className="pc-switches">
            <span className="pc-switch-group">
              <span className="pc-switch-cap">岛展示</span>
              <MiniSwitch
                on={account.in_island}
                label={`${account.alias} 岛展示`}
                title={
                  account.in_island
                    ? "在灵动岛轮播中展示，点击移出"
                    : "点击加入灵动岛轮播"
                }
                onChange={() => onToggleInIsland?.()}
              />
            </span>
            <span className="pc-switch-group">
              <span className="pc-switch-cap">启用</span>
              <button
                type="button"
                role="switch"
                aria-checked={account.enabled}
                aria-label={`${account.alias} 启停`}
                title={account.enabled ? "停用（保留凭据）" : "启用"}
                className={`pc-switch-btn${account.enabled ? " pc-switch-on" : ""}`}
                onClick={() => onToggle?.(!account.enabled)}
              >
                <span className="pc-switch-thumb" />
              </button>
            </span>
          </span>
        )}
      </div>
      {/* 次行（仅设置页形态）：品牌色点 + 厂商名 + 来源徽标 */}
      {!hideSwitches && <div className="pc-sub">{identity}</div>}
      {/* 额度页 detail 插槽接管额度体（进度条/金额大字）；设置页保持一行摘要。
          凭据失效红字与"历史"小标的悬浮说明两条路径都保留（额度页历史小标
          由 detail 内容自带，此处红字行共用） */}
      {detail !== undefined ? (
        detail
      ) : (
        <div className="pc-summary">
          <span className="pc-summary-main">
            {summaryOf(account, thresholds)}
            {account.cred_error && <span className="pc-stale">历史</span>}
          </span>
          <span className="pc-summary-time" title={summaryTitleOf(account)}>
            {fmtDT(fetchedAtOf(account))}
          </span>
        </div>
      )}
      {account.cred_error && (
        <div className="pc-cred-err">
          {account.cred_error}
          {onCredAction && (
            <button type="button" className="pc-cred-fix" onClick={onCredAction}>
              去设置页处理 →
            </button>
          )}
        </div>
      )}
      {account.base_override && (
        <div className="pc-base" title={account.base_override}>
          接口：{account.base_override}
        </div>
      )}
      {testing && <div className="pc-test pc-test-busy">检测中…</div>}
      {!testing && testResult && (
        <div className={`pc-test ${testResult.ok ? "pc-test-ok" : "pc-test-err"}`}>
          {testResult.ok
            ? `✓ ${testResult.text}`
            : dupWithCredError
              ? "✗ 检测失败，原因同上"
              : `✗ ${testResult.text}`}
        </div>
      )}
      {/* 额度页 actions 插槽接管脚注（时间＋状态徽标＋刷新）；设置页保持管理按钮行 */}
      {actions !== undefined ? (
        actions
      ) : (
        <div className="pc-foot">
          <button
            type="button"
            className="pc-btn"
            disabled={testing || !account.enabled}
            onClick={onTest}
          >
            检测
          </button>
          <button type="button" className="pc-btn" onClick={onEdit}>
            编辑
          </button>
          <button
            type="button"
            className={`pc-btn${confirming ? " pc-btn-danger" : ""}`}
            title={confirming ? "再次点击确认删除（同时清除钥匙串凭据）" : "删除该实例"}
            onClick={askDelete}
          >
            {confirming ? "确认删除？" : "删除"}
          </button>
        </div>
      )}
    </div>
  );
}
