/**
 * 「额度与凭据」分区（2026-10-03 四轮审查拆分自 Settings.tsx，JSX/行为不变）：
 * 全局配置行（自动查询/阈值/警戒线/预算/档位）＋实例卡片流＋添加/编辑弹窗。
 * 拆分动机：本分区是设置页最大状态簇（阈值输入/预算输入的每次击键原先触发
 * 全页 reconcile），收敛后击键只重渲染本分区。
 * 自加载：挂载时读本分区设置切片＋拉厂商清单与实例列表。
 */
import { useEffect, useMemo, useState } from "react";
import { emit } from "@tauri-apps/api/event";
import { ChevronIcon, GripIcon, PlusIcon } from "../shared/icons";
import ProviderCard from "../shared/ProviderCard";
import { invokeReady } from "../shared/invokeReady";
import { SortableBox, SortableItem, SortableList, moveIn } from "../shared/Sortable";
import {
  deleteAccount,
  listAccounts,
  reorderAccounts,
  setAccountEnabled,
  setAccountInIsland,
  testAccount,
  type AccountOverview,
  type KindEntryView,
  type ProviderKindView,
  type TestResult,
} from "../shared/providerTypes";
import {
  blurOnEnter,
  CLAUDE_PLAN_OPTIONS,
  QUOTA_SECTION_ID,
  Row,
  Section,
  Switch,
} from "./widgets";
import ProviderFormModal from "./ProviderFormModal";

export default function QuotaSection({
  saveKey,
  showToast,
}: {
  /** 单键落库（失败 toast），由编排层提供保持单一来源 */
  saveKey: (key: string, value: string) => Promise<boolean>;
  showToast: (text: string, kind: "ok" | "error") => void;
}) {
  // —— 阈值/警戒线/预算（每项改动即时落库，无统一保存按钮） ——
  const [warn, setWarn] = useState("80");
  const [danger, setDanger] = useState("95");
  // 阈值校验错误（I3 改造）：文本就近显示 + 记录出错的是哪个框（输入框边框标红定位）
  const [thresholdError, setThresholdError] = useState<{
    text: string;
    warn: boolean;
    danger: boolean;
  } | null>(null);
  // 自动查询总开关（缺省开，与后端 quota_fetch_enabled 默认一致）
  const [quotaFetch, setQuotaFetch] = useState(true);
  // Claude 订阅档位（M3-12）：claude-local 实例的 5h 限额依据，缺省自动探测
  const [claudePlan, setClaudePlan] = useState("auto");
  // 余额警戒线（缺省 ¥10，与 06 §7.1 一致；M3-6 托盘/岛告急口径消费）
  const [balWarn, setBalWarn] = useState("10");
  const [balWarnError, setBalWarnError] = useState("");
  // 美元账户余额警戒线（M3-7 按币种分设，OpenRouter 等余额按 USD 计）
  const [balWarnUsd, setBalWarnUsd] = useState("5");
  const [balWarnUsdError, setBalWarnUsdError] = useState("");
  // 消费预算（#23）：日/月估算成本线（美元；0=不设），报表页预算条消费
  const [budgetDaily, setBudgetDaily] = useState("0");
  const [budgetMonthly, setBudgetMonthly] = useState("0");
  const [budgetError, setBudgetError] = useState("");
  // 已成功提交的阈值（五轮审查批次一）：实例卡分色只认「已落库的值」——
  // 原先直接取输入框草稿值下发，输入 200（校验会拒）时卡片立即按 200 分色，
  // 「未保存先变色」是错觉；失败/未提交的草稿不再驱动 UI
  const [committedT, setCommittedT] = useState({ warn: 80, danger: 95, balanceWarn: 10, balanceWarnUsd: 5 });
  // —— 供应商实例管理（M3-4，06-PLAN §7） ——
  // 厂商清单与实例列表（进入页面拉一次；实例操作后手动重拉）
  const [kinds, setKinds] = useState<ProviderKindView[]>([]);
  /** 选择器条目清单（2026-10-10 双站变体展开投影；添加弹窗数据源） */
  const [entries, setEntries] = useState<KindEntryView[]>([]);
  const [accounts, setAccounts] = useState<AccountOverview[]>([]);
  // 厂商折叠分组（2026-09-29 审查修复 #13）：>3 家时启用，收起的 kind_id 集合
  // （会话级状态，与额度页同款不持久化）；默认全展开
  const [instCollapsed, setInstCollapsed] = useState<Set<string>>(new Set());
  const toggleInstGroup = (kid: string) =>
    setInstCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(kid)) next.delete(kid);
      else next.add(kid);
      return next;
    });
  // 添加/编辑弹窗：editing=null 为新增；打开即挂载 ProviderFormModal（状态自含）
  const [formOpen, setFormOpen] = useState(false);
  const [editing, setEditing] = useState<AccountOverview | null>(null);
  // 检测进行中的实例 id 集合与各实例最近检测结果。
  // 集合而非单值（四轮审查）：单值在并发检测（保存并检测×多卡手点）时会互相
  // 顶替——B 开始即清掉 A 的进行中态（按钮提前解禁可重复触发），先完成者的
  // finally 又清掉 B 的；集合各卡独立增删
  const [testingIds, setTestingIds] = useState<Set<string>>(new Set());
  const [testResults, setTestResults] = useState<Record<string, TestResult>>({});

  // 自加载本分区设置切片
  useEffect(() => {
    (async () => {
      try {
        const s = await invokeReady<Record<string, string>>("get_settings");
        if (s.threshold_warn) setWarn(s.threshold_warn);
        if (s.threshold_danger) setDanger(s.threshold_danger);
        if (s.quota_fetch_enabled !== undefined) setQuotaFetch(s.quota_fetch_enabled !== "0");
        if (s.quota_balance_warn) setBalWarn(s.quota_balance_warn);
        if (s.quota_balance_warn_usd) setBalWarnUsd(s.quota_balance_warn_usd);
        if (s.budget_daily_usd) setBudgetDaily(s.budget_daily_usd);
        if (s.budget_monthly_usd) setBudgetMonthly(s.budget_monthly_usd);
        if (s.claude_plan) setClaudePlan(s.claude_plan);
        // 已提交阈值与输入框同步初始化（脏值回落默认，与展示口径一致）
        const num = (v: string | undefined, d: number) => {
          const n = Number(v);
          return v !== undefined && Number.isFinite(n) ? n : d;
        };
        setCommittedT({
          warn: num(s.threshold_warn, 80),
          danger: num(s.threshold_danger, 95),
          balanceWarn: num(s.quota_balance_warn, 10),
          balanceWarnUsd: num(s.quota_balance_warn_usd, 5),
        });
      } catch {
        /* 加载失败保持默认值 */
      }
    })();
  }, []);

  /** 实例列表加载（进页拉一次；厂商清单/选择器条目都是静态注册表投影，随之
   *  一起拉取——弹窗 KindPicker 与卡片品牌色都依赖）。
   *  走 invokeReady 重试：挂载可能早于后端 setup 完成（state 未 managed） */
  const refreshProviders = async () => {
    try {
      const [ks, es, accs] = await Promise.all([
        invokeReady<ProviderKindView[]>("provider_kinds"),
        invokeReady<KindEntryView[]>("provider_kind_entries"),
        invokeReady<AccountOverview[]>("provider_overview"),
      ]);
      setKinds(ks);
      setEntries(es);
      setAccounts(accs);
    } catch (e) {
      showToast(`供应商实例加载失败：${e}`, "error");
    }
  };

  useEffect(() => {
    void refreshProviders();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** 实例列表重拉（增删改后调用） */
  const refreshAccounts = async () => {
    try {
      setAccounts(await listAccounts());
    } catch (e) {
      showToast(`实例列表刷新失败：${e}`, "error");
    }
  };

  /** 实例表变更广播（M3-5 E2）：额度页等订阅方即时重拉，不再依赖手动刷新。
   *  与 agents-changed 同款模式；emit 无订阅方时静默无害 */
  const notifyAccountsChanged = () => {
    void emit("provider-accounts-changed");
  };

  /** 拖拽重排落库（2026-10-03）：乐观更新→provider_account_reorder 全量重写
   *  sort_order（0011 迁移列）→失败重拉回滚＋toast；成功广播
   *  provider-accounts-changed（额度页即时重拉；岛轮播/托盘随下一轮快照跟随） */
  const persistOrder = async (next: AccountOverview[]) => {
    setAccounts(next);
    try {
      await reorderAccounts(next.map((a) => a.id));
      notifyAccountsChanged();
    } catch (e) {
      showToast(`排序保存失败：${e}`, "error");
      await refreshAccounts();
    }
  };

  /** 卡片落位（平铺全局拖/分组组内拖共用）：在全量 id 序上 moveIn——分组形态
   *  下同组实例在序中连续，组内交换不影响其他组的块结构，两种形态同一实现 */
  const dropCardOn = (activeId: string, overId: string) => {
    const ids = moveIn(
      accounts.map((a) => a.id),
      activeId,
      overId,
    );
    if (!ids) return;
    const byId = new Map(accounts.map((a) => [a.id, a]));
    persistOrder(
      ids.map((id) => byId.get(id)).filter((a): a is AccountOverview => a !== undefined),
    );
  };

  /** 组头落位：整组作为连续块移动（组序＝组内首实例的序，分组只是视图） */
  const dropGroupOn = (fromKid: string, toKid: string) => {
    const groups = new Map<string, AccountOverview[]>();
    for (const a of accounts) {
      const g = groups.get(a.kind_id);
      if (g) g.push(a);
      else groups.set(a.kind_id, [a]);
    }
    const kids = moveIn([...groups.keys()], fromKid, toKid);
    if (!kids) return;
    persistOrder(kids.flatMap((kid) => groups.get(kid) ?? []));
  };

  /** 自动查询总开关：即存即生效（调度器每 tick 现读，最多一个周期内生效）。
   *  失败回滚（五轮审查批次一，对齐 toggleAutoStart 模式） */
  const toggleQuotaFetch = async (v: boolean) => {
    setQuotaFetch(v);
    if (!(await saveKey("quota_fetch_enabled", v ? "1" : "0"))) setQuotaFetch(!v);
  };

  /** 阈值全字段广播（2026-10-03 审查修复）：三个提交点（百分比/人民币线/美元线）
   *  统一发当前全量值，监听端按字段各自校验合并——余额警戒线原先只落库不广播，
   *  岛/托盘继续用旧线判定直到重启，与 9-29「阈值即时生效」意图冲突。
   *  已知取舍（四轮审查复核认定可接受）：未提交字段的 ?? 兜底取自输入框当前值——
   *  用户改了 warn 未失焦就提交余额线时，warn 草稿会先于落库短暂下发；岛端
   *  按字段校验合并能拦非法值，合法草稿在随后的失焦提交中自然对齐 */
  const emitThresholds = (patch: {
    warn?: number;
    danger?: number;
    balanceWarn?: number;
    balanceWarnUsd?: number;
  }) => {
    emit("thresholds-changed", {
      warn: patch.warn ?? Number(warn),
      danger: patch.danger ?? Number(danger),
      balanceWarn: patch.balanceWarn ?? Number(balWarn),
      balanceWarnUsd: patch.balanceWarnUsd ?? Number(balWarnUsd),
    }).catch(() => {});
  };

  /** 阈值失焦校验并落库（I3 改造）：两个值都合法且琥珀 < 红色才写入，否则就近提示
   *  并把出错的输入框边框标红；输入中即时清错复原（视为开始修正），失焦再校验。
   *  2026-09-29 分组优化：落库成功后广播 thresholds-changed——岛/托盘即时更新，
   *  不再要求重开窗口（「重启后生效」文案随之删除） */
  const commitThresholds = async () => {
    const w = Number(warn);
    const d = Number(danger);
    const inRange = (v: number) => Number.isFinite(v) && v > 0 && v <= 100;
    if (!inRange(w) || !inRange(d)) {
      setThresholdError({
        text: "阈值须为 1～100 的数字",
        warn: !inRange(w),
        danger: !inRange(d),
      });
      return;
    }
    if (w >= d) {
      setThresholdError({ text: "琥珀阈值须小于红色阈值", warn: true, danger: true });
      return;
    }
    setThresholdError(null);
    // 门控广播（五轮审查批次一）：任一键保存失败即返回，不广播——岛/托盘
    // 分色维持旧值与 toast「保存失败」自洽（原先失败仍照发＝假生效）
    if (!(await saveKey("threshold_warn", warn))) return;
    if (!(await saveKey("threshold_danger", danger))) return;
    setCommittedT((c) => ({ ...c, warn: w, danger: d }));
    emitThresholds({ warn: w, danger: d });
  };

  /** 余额警戒线失焦校验并落库：非负数字（后端同规则兜底）；输入即清错。
   *  落库后走 emitThresholds 全字段广播（岛/托盘即时生效） */
  const commitBalWarn = async () => {
    const v = Number(balWarn);
    if (!Number.isFinite(v) || v < 0) {
      setBalWarnError("警戒线须为不小于 0 的数字");
      return;
    }
    setBalWarnError("");
    if (!(await saveKey("quota_balance_warn", String(v)))) return; // 失败不广播（同上）
    setCommittedT((c) => ({ ...c, balanceWarn: v }));
    emitThresholds({ balanceWarn: v });
  };

  /** 美元账户余额警戒线失焦校验并落库（M3-7，规则同人民币线） */
  const commitBalWarnUsd = async () => {
    const v = Number(balWarnUsd);
    if (!Number.isFinite(v) || v < 0) {
      setBalWarnUsdError("警戒线须为不小于 0 的数字");
      return;
    }
    setBalWarnUsdError("");
    if (!(await saveKey("quota_balance_warn_usd", String(v)))) return; // 失败不广播（同上）
    setCommittedT((c) => ({ ...c, balanceWarnUsd: v }));
    emitThresholds({ balanceWarnUsd: v });
  };

  /** 消费预算失焦校验并落库（#23）：两输入同规则，任一非法红字拦截 */
  const commitBudget = async () => {
    const d = Number(budgetDaily);
    const m = Number(budgetMonthly);
    if (!Number.isFinite(d) || d < 0 || !Number.isFinite(m) || m < 0) {
      setBudgetError("预算须为不小于 0 的数字");
      return;
    }
    setBudgetError("");
    await saveKey("budget_daily_usd", String(d));
    await saveKey("budget_monthly_usd", String(m));
  };

  /** 岛展示集切换：本地即时翻转＋落库，失败回滚（软上限 5 由行描述提示） */
  const toggleInIsland = async (a: AccountOverview) => {
    const next = !a.in_island;
    setAccounts((prev) => prev.map((x) => (x.id === a.id ? { ...x, in_island: next } : x)));
    try {
      await setAccountInIsland(a.id, next);
      notifyAccountsChanged();
    } catch (e) {
      showToast(`保存失败：${e}`, "error");
      setAccounts((prev) => prev.map((x) => (x.id === a.id ? { ...x, in_island: !next } : x)));
    }
  };

  /** 启停实例：停用不删除（凭据保留），失败回滚 */
  const toggleAccount = async (a: AccountOverview) => {
    const next = !a.enabled;
    setAccounts((prev) => prev.map((x) => (x.id === a.id ? { ...x, enabled: next } : x)));
    try {
      await setAccountEnabled(a.id, next);
      notifyAccountsChanged();
      showToast(next ? `已启用「${a.alias}」` : `已停用「${a.alias}」，凭据保留`, "ok");
    } catch (e) {
      showToast(`操作失败：${e}`, "error");
      setAccounts((prev) => prev.map((x) => (x.id === a.id ? { ...x, enabled: !next } : x)));
    }
  };

  /** 卡上检测：三态行内反馈写回 testResults（检测失败只展示不阻塞） */
  const runTest = async (id: string) => {
    setTestingIds((cur) => new Set(cur).add(id));
    try {
      const text = await testAccount(id);
      setTestResults((prev) => ({ ...prev, [id]: { ok: true, text } }));
    } catch (e) {
      setTestResults((prev) => ({ ...prev, [id]: { ok: false, text: String(e) } }));
    } finally {
      // 只移除自己（四轮审查）：无条件清空会误伤其他进行中的检测
      setTestingIds((cur) => {
        const next = new Set(cur);
        next.delete(id);
        return next;
      });
    }
  };

  /** 删除实例（二次确认已在卡内完成）：删除后重拉列表 */
  const removeAccount = async (a: AccountOverview) => {
    try {
      await deleteAccount(a.id);
      showToast(`已删除「${a.alias}」，历史快照保留`, "ok");
      await refreshAccounts();
      notifyAccountsChanged();
    } catch (e) {
      showToast(`删除失败：${e}`, "error");
    }
  };

  /** 分组（2026-10-03 起保持用户拖拽序：组序＝组内首实例的序；后端
   *  list_provider_accounts 是唯一顺序源，此处仅按 kind_id 聚合保序） */
  const instGroups = useMemo(() => {
    const map = new Map<string, AccountOverview[]>();
    for (const a of accounts) {
      const list = map.get(a.kind_id);
      if (list) list.push(a);
      else map.set(a.kind_id, [a]);
    }
    return [...map.entries()];
  }, [accounts]);

  const renderInstCard = (a: AccountOverview) => (
    <ProviderCard
      key={a.id}
      account={a}
      kind={kinds.find((k) => k.id === a.kind_id)}
      thresholds={committedT}
      testing={testingIds.has(a.id)}
      testResult={testResults[a.id]}
      onToggle={() => toggleAccount(a)}
      onToggleInIsland={() => toggleInIsland(a)}
      onTest={() => runTest(a.id)}
      onEdit={() => {
        setEditing(a);
        setFormOpen(true);
      }}
      onDelete={() => removeAccount(a)}
    />
  );

  return (
    <Section
      title="额度与凭据"
      id={QUOTA_SECTION_ID}
      desc={
        accounts.filter((a) => a.in_island).length > 5
          ? `各 AI 平台的余额与窗口用量自动查询；在岛展示 ${accounts.filter((a) => a.in_island).length} 个实例，已超建议上限 5 个`
          : "各 AI 平台的余额与窗口用量自动查询，按实例管理"
      }
    >
      <Row
        title="自动查询额度"
        desc="按 5 分钟周期刷新全部启用实例的额度数据"
        tip="单实例查询失败按 5→10→20 分钟指数退避（最长 60 分钟），不影响其他实例"
      >
        <Switch checked={quotaFetch} onChange={toggleQuotaFetch} />
      </Row>

      <Row
        title="额度提醒阈值"
        desc="窗口用量达到琥珀阈值（%）开始提醒，达到红色阈值转为告警"
        error={thresholdError?.text}
      >
        <div className="st-th">
          <i className="st-dot st-dot-amber" />
          <input
            className={`st-input st-input-num${thresholdError?.warn ? " st-input-err" : ""}`}
            type="number"
            min={1}
            max={100}
            value={warn}
            onChange={(e) => {
              setWarn(e.target.value);
              // 输入即视为开始修正，即时撤掉红框与错误文案（失焦再校验）
              setThresholdError(null);
            }}
            onBlur={commitThresholds}
            onKeyDown={blurOnEnter}
            aria-label="琥珀提醒阈值"
          />
        </div>
        <div className="st-th">
          <i className="st-dot st-dot-red" />
          <input
            className={`st-input st-input-num${thresholdError?.danger ? " st-input-err" : ""}`}
            type="number"
            min={1}
            max={100}
            value={danger}
            onChange={(e) => {
              setDanger(e.target.value);
              setThresholdError(null);
            }}
            onBlur={commitThresholds}
            onKeyDown={blurOnEnter}
            aria-label="红色告警阈值"
          />
        </div>
      </Row>
      <Row
        title="余额警戒线"
        desc="实例余额低于警戒线时，托盘与岛将该实例标为告急"
        tip="人民币实例比 ¥ 线，美元实例（如 OpenRouter）比 $ 线，其余币种不判告急"
        error={balWarnError || balWarnUsdError || undefined}
      >
        <div className="st-th">
          <span className="st-bal-sym">¥</span>
          <input
            className={`st-input st-input-num${balWarnError ? " st-input-err" : ""}`}
            type="number"
            min={0}
            step={1}
            value={balWarn}
            onChange={(e) => {
              setBalWarn(e.target.value);
              setBalWarnError("");
            }}
            onBlur={commitBalWarn}
            onKeyDown={blurOnEnter}
            aria-label="人民币余额警戒线"
          />
          <span className="st-bal-sym">$</span>
          <input
            className={`st-input st-input-num${balWarnUsdError ? " st-input-err" : ""}`}
            type="number"
            min={0}
            step={1}
            value={balWarnUsd}
            onChange={(e) => {
              setBalWarnUsd(e.target.value);
              setBalWarnUsdError("");
            }}
            onBlur={commitBalWarnUsd}
            onKeyDown={blurOnEnter}
            aria-label="美元余额警戒线"
          />
        </div>
      </Row>
      {/* 消费预算（2026-09-29 审查新增 #23）：日/月估算成本超线时报表页标红；
          0=不设。与报表「估算成本」同口径（当前单价×用量，USD） */}
      <Row
        title="消费预算"
        desc="今日/本月估算成本（美元）超过预算时，报表页预算条标红；0 表示不设"
        tip="与报表「估算成本」同口径：按当前单价 × 历史用量估算（价格变动不追溯）；订阅制额度不折算在内"
        error={budgetError || undefined}
      >
        <div className="st-th">
          <span className="st-bal-sym">日 $</span>
          <input
            className={`st-input st-input-num${budgetError ? " st-input-err" : ""}${Number(budgetDaily) === 0 ? " st-input-muted" : ""}`}
            type="number"
            min={0}
            step={1}
            value={budgetDaily}
            onChange={(e) => {
              setBudgetDaily(e.target.value);
              setBudgetError("");
            }}
            onBlur={commitBudget}
            onKeyDown={blurOnEnter}
            aria-label="每日消费预算（美元）"
          />
          <span className="st-bal-sym">月 $</span>
          <input
            className={`st-input st-input-num${budgetError ? " st-input-err" : ""}${Number(budgetMonthly) === 0 ? " st-input-muted" : ""}`}
            type="number"
            min={0}
            step={1}
            value={budgetMonthly}
            onChange={(e) => {
              setBudgetMonthly(e.target.value);
              setBudgetError("");
            }}
            onBlur={commitBudget}
            onKeyDown={blurOnEnter}
            aria-label="每月消费预算（美元）"
          />
        </div>
      </Row>
      {/* 档位行条件渲染（2026-09-29 设置页分组优化）：只服务 claude-local
          实例的 5h 推算——没建该实例时是死配置，隐藏避免误导 */}
      {accounts.some((a) => a.kind_id === "claude-local") && (
        <Row
          title="Claude 订阅档位"
          desc="「Claude 订阅」实例专用的 5 小时窗口限额依据"
          tip="自动探测＝按近 8 天已完成块用量估计限额；Pro/Max 档为社区测算值；切换后重启额度查询生效"
        >
          <select
            className="st-input st-input-sm"
            value={claudePlan}
            onChange={(e) => {
              const prev = claudePlan;
              setClaudePlan(e.target.value);
              // 失败回滚下拉显示（五轮审查批次一，同保留时长下拉）
              void saveKey("claude_plan", e.target.value).then((ok) => {
                if (!ok) setClaudePlan(prev);
              });
            }}
            aria-label="Claude 订阅档位"
          >
            {CLAUDE_PLAN_OPTIONS.map((o) => (
              <option key={o.value} value={o.value}>
                {o.value === "auto" ? `${o.label}（默认）` : o.label}
              </option>
            ))}
          </select>
        </Row>
      )}
      {/* 实例卡片流（06 §7.1）：品牌色条 + 额度摘要 + 岛展示/启停/检测/编辑/删除；
          末尾虚线卡为添加入口（UX 改版：表单迁入弹窗，页面只留入口）。
          2026-09-28 重排垫底：全局配置行（查询/警戒线/阈值/档位）归拢在前，
          实例卡片流整体置后，消除原"行-卡-行"夹心结构。
          2026-09-29 审查修复 #13：>3 家厂商时按厂商折叠分组（与额度页同款
          形态）——实例上双位数后平铺创建序已不可扫描；≤3 家保持平铺零回退。
          2026-10-03 拖拽排序（0011 迁移列）：平铺态全局拖；分组态组头拖＝
          整组平移、组内卡片拖＝组内重排（组头与各组卡片区是独立的
          DndContext，跨组拖拽在结构上不存在）；全量 id 序整体落库 */}
      <div className="st-insts">
        {instGroups.length <= 3 ? (
          <SortableList
            ids={accounts.map((a) => a.id)}
            onDragEnd={(e) => {
              if (e.over) dropCardOn(String(e.active.id), String(e.over.id));
            }}
          >
            {accounts.map((a) => (
              <SortableItem key={a.id} id={a.id}>
                {renderInstCard(a)}
              </SortableItem>
            ))}
          </SortableList>
        ) : (
          <SortableList
            ids={instGroups.map(([kid]) => kid)}
            onDragEnd={(e) => {
              if (e.over) dropGroupOn(String(e.active.id), String(e.over.id));
            }}
          >
            {instGroups.map(([kid, list]) => {
              const k = kinds.find((x) => x.id === kid);
              const open = !instCollapsed.has(kid);
              return (
                <SortableBox key={kid} id={kid}>
                  {({ ref, handleProps }) => (
                    <div className="st-inst-group">
                      <button
                        type="button"
                        ref={ref}
                        className="st-inst-group-head"
                        onClick={() => toggleInstGroup(kid)}
                        title="点击折叠/展开（Enter）；拖动调整该厂商组的位置（键盘：空格拾起、方向键移动）"
                        {...handleProps}
                      >
                        <span className="st-inst-grip" title="拖动排序（整组移动）">
                          <GripIcon />
                        </span>
                        <span className="st-inst-chevron"><ChevronIcon dir={open ? "down" : "right"} /></span>
                        <i className="pc-dot" style={{ background: k?.color ?? "#8a8f98" }} />
                        <span>{k?.name ?? kid}</span>
                        <span className="st-inst-count">{list.length} 个实例</span>
                      </button>
                      {open && (
                        <SortableList
                          ids={list.map((a) => a.id)}
                          onDragEnd={(e) => {
                            if (e.over) dropCardOn(String(e.active.id), String(e.over.id));
                          }}
                        >
                          <div className="st-inst-group-cards">
                            {list.map((a) => (
                              <SortableItem key={a.id} id={a.id}>
                                {renderInstCard(a)}
                              </SortableItem>
                            ))}
                          </div>
                        </SortableList>
                      )}
                    </div>
                  )}
                </SortableBox>
              );
            })}
          </SortableList>
        )}
        <button type="button" className="st-insts-add" onClick={() => { setEditing(null); setFormOpen(true); }}>
          <span className="st-insts-add-plus">
            <PlusIcon />
          </span>
          {accounts.length === 0 ? "添加第一个供应商" : "添加供应商"}
        </button>
      </div>

      {/* 添加/编辑供应商弹窗：打开即挂载（表单状态在弹窗组件内自含） */}
      {formOpen && (
        <ProviderFormModal
          entries={entries}
          accounts={accounts}
          editing={editing}
          onClose={() => setFormOpen(false)}
          onSaved={async () => {
            await refreshAccounts();
            notifyAccountsChanged();
          }}
          runTest={runTest}
          showToast={showToast}
        />
      )}
    </Section>
  );
}
