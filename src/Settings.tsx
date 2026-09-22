/**
 * 设置页（T11 / 2026-09-17 重构）：分区卡片 + 设置项即时生效
 * - 布局：统一"设置行"（固定标题 + 固定描述 + 右侧控件），按使用频率分五节；
 *   分区头为主标题 + 副标题同行（副标题不换行，窗口最小宽度据此设下限）
 * - 交互：改动即存即生效（无保存按钮、不再保存后自动关窗）；开关行标题/描述固定，
 *   状态由 Switch 与徽标表达，文案不随选中态变化（遵循 Fluent 开关文案规范）
 * - 反馈：校验错误内联显示在出错行正下方；操作结果用顶部 toast（成功 2.5s 自动消失，
 *   失败常驻直到下一次提示）；凭据/阈值"重启生效"的事实写入行描述，不做打扰式弹提示
 * - Key 回显已存值（2026-09-17 所有者要求，推翻原"不回显"决策）；
 *   凭据来源模式化（2026-09-18）：自动发现 / 手动指定显式二选一，取代原"清空失焦不覆盖"决策——
 *   手动模式留空失焦 = 未修改（防误清空），清除 Key = 切回"自动发现"（显式写空值回落发现链）
 * - 界面文案一律简体中文标点（2026-09-17 验收建议 3）
 */
import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import { AGENT_COLORS, AGENT_DEFS, HOOKS_AGENTS } from "./shared/types";
import { asThemeMode, useTheme, type ThemeMode } from "./shared/theme";
import {
  ISLAND_OPACITY_DEFAULT,
  ISLAND_OPACITY_EVENT,
  ISLAND_OPACITY_KEY,
  ISLAND_OPACITY_MAX,
  ISLAND_OPACITY_MIN,
  asIslandOpacity,
} from "./shared/islandOpacity";
import "./settings.css";

/** 数据保留时长选项（天），按时长降序；12 个月 = 365 天，与后端"未设置默认保留 1 年"一致 */
const CLEANUP_OPTIONS: { label: string; days: number }[] = [
  { label: "12 个月", days: 365 },
  { label: "6 个月", days: 180 },
  { label: "3 个月", days: 90 },
  { label: "1 个月", days: 30 },
  { label: "15 天", days: 15 },
  { label: "7 天", days: 7 },
  { label: "1 天", days: 1 },
];

/** 默认保留时长（12 个月；与后端清理默认值 365 天一致） */
const CLEANUP_DEFAULT_DAYS = 365;

/** 托盘左键动作（与 Rust 端 tray_left_action 同源约定）：none=无操作（默认档） */
type TrayLeftAction = "none" | "toggle" | "menu";
const TRAY_LEFT_DEFAULT: TrayLeftAction = "none";

/** 分区卡片：主标题 + 副标题同行（主/副标题关系），下方为设置行列表 */
function Section({
  title,
  desc,
  children,
}: {
  title: string;
  desc?: string;
  children: React.ReactNode;
}) {
  return (
    <section className="st-section">
      <div className="st-sec-head">
        <div className="st-sec-title">{title}</div>
        {desc && <div className="st-sec-desc">{desc}</div>}
      </div>
      <div className="st-sec-body">{children}</div>
    </section>
  );
}

/** 设置行：左 = 固定标题（+可选徽标）+ 固定描述，右 = 控件；error 就近显示在本行下方 */
function Row({
  title,
  badge,
  desc,
  error,
  tall,
  children,
}: {
  title: string;
  badge?: React.ReactNode;
  desc?: string;
  error?: string;
  /** tall：宽控件（输入框）放到文字下方独占一行 */
  tall?: boolean;
  children?: React.ReactNode;
}) {
  return (
    <div className="st-row-item">
      <div className="st-row-main">
        <div className="st-row-text">
          <div className="st-row-title">
            {title}
            {badge}
          </div>
          {desc && <div className="st-row-desc">{desc}</div>}
        </div>
        {!tall && children != null && <div className="st-row-control">{children}</div>}
      </div>
      {tall && children != null && (
        <div className="st-row-control st-row-control-tall">{children}</div>
      )}
      {error && <div className="st-row-error">{error}</div>}
    </div>
  );
}

/** 滑动开关：状态由开/关位置表达，标题文案保持固定；small 为行内小号变体（Agent 行精确开关），disabled 用于 busy/前置条件不满足 */
function Switch({
  checked,
  onChange,
  disabled,
  title,
  small,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
  title?: string;
  small?: boolean;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      className={`st-switch${small ? " st-switch-sm" : ""}${checked ? " st-switch-on" : ""}`}
      disabled={disabled}
      title={title}
      onClick={() => onChange(!checked)}
    >
      <span className="st-switch-thumb" />
    </button>
  );
}

/** 分段选择（泛化）：点击即切换，供主题三档 / 凭据来源两档复用 */
function Segmented<T extends string>({
  value,
  onChange,
  options,
  ariaLabel,
}: {
  value: T;
  onChange: (v: T) => void;
  options: { value: T; label: string }[];
  ariaLabel: string;
}) {
  return (
    <div className="st-seg" role="radiogroup" aria-label={ariaLabel}>
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          role="radio"
          aria-checked={value === o.value}
          className={value === o.value ? "st-seg-item st-seg-on" : "st-seg-item"}
          onClick={() => onChange(o.value)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

/** 状态徽标：点色 + 文案（已启用=强调色，未启用=灰），随主题换色 */
function Badge({ on, text }: { on: boolean; text: string }) {
  return (
    <span className={on ? "st-badge st-badge-on" : "st-badge"}>
      <i className="st-badge-dot" />
      {text}
    </span>
  );
}

/** 眼睛图标（off=true 画斜线，表示当前隐藏中） */
function EyeIcon({ off }: { off: boolean }) {
  return (
    <svg
      width="15"
      height="15"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d="M1 12s4-7 11-7 11 7 11 7-4 7-11 7S1 12 1 12z" />
      <circle cx="12" cy="12" r="3" />
      {off && <line x1="4" y1="4" x2="20" y2="20" />}
    </svg>
  );
}

export default function Settings() {
  // —— 表单状态（每项改动即时落库，无统一保存按钮） ——
  const [glmBase, setGlmBase] = useState("https://open.bigmodel.cn");
  const [glmToken, setGlmToken] = useState(""); // 输入框内容；回显已存 Key（2026-09-17 所有者要求）
  const savedTokenRef = useRef(""); // 最近一次落库的 Key：失焦时比对，未变更不重复落库/提示
  // 凭据来源模式（2026-09-18 模式化改造）：auto = 自动发现链（env → claude-menu），manual = 手动指定。
  // 与后端判定同源：glm_token 非空即手动，清空即回落自动，无需独立设置键
  const [credMode, setCredMode] = useState<"auto" | "manual">("auto");
  const [showToken, setShowToken] = useState(false); // 明文/密文切换
  const [tokenFrom, setTokenFrom] = useState(""); // 已生效凭据来源（聚合器启动时写入）
  const [warn, setWarn] = useState("80");
  const [danger, setDanger] = useState("95");
  const [thresholdError, setThresholdError] = useState("");
  // 数据保留周期（未设置时后端按 1 年清理，前端默认值与之对齐）
  const [cleanupDays, setCleanupDays] = useState(CLEANUP_DEFAULT_DAYS);
  // hooks 安装状态（M2-6/7 多 Agent：agent id → 是否已注入；三家各自独立）
  const [hooksOn, setHooksOn] = useState<Record<string, boolean>>({});
  const [hookBusy, setHookBusy] = useState<Record<string, boolean>>({}); // 各家注入/卸载进行中，防连点
  // 开发者模式（缺省=关；开启后 Rust 端即时切到 Debug 级日志，免重启）
  const [devMode, setDevMode] = useState(false);
  const [autoStart, setAutoStart] = useState(false);
  // 灵动岛贴边自动隐藏（缺省=开，与 Rust 端 autohide_enabled 的默认一致）
  const [autoHide, setAutoHide] = useState(true);
  // 岛背景不透明度（缺省 72%，与历史深色胶囊 alpha 一致；面板/隐藏态按偏移派生）
  const [islandOpacity, setIslandOpacity] = useState(ISLAND_OPACITY_DEFAULT);
  // 不透明度落库防抖定时器：拖动中只广播不写库，停手 300ms 后落一次盘
  const opacitySaveTimer = useRef<number | null>(null);
  // 悬停自动展开信息卡片（缺省=开；关闭时点击岛展开/收回）
  const [hoverCard, setHoverCard] = useState(true);
  // 监控的 Agent 列表（缺省全选；勾选才采集/监控/展示）
  const [agents, setAgents] = useState<string[]>(AGENT_DEFS.map((a) => a.id));
  // Agent 自定义身份色（未自定义的用系统默认色；隐藏态色块/面板徽标共用）
  const [agentColors, setAgentColors] = useState<Record<string, string>>({});
  // 已落库的自定义色：拖拽选色过程中实时改 UI 但不落库，失焦校验撞色后按此回滚
  const savedColorsRef = useRef<Record<string, string>>({});
  const [colorError, setColorError] = useState("");
  // 主题模式（跟随系统/深色/浅色，点击即切换全窗口预览）
  const [themeMode, setThemeMode] = useState<ThemeMode>("system");
  // 托盘左键动作（缺省=无操作，与 Rust 端 tray_left_action 的默认一致；右键恒为菜单）
  const [trayLeft, setTrayLeft] = useState<TrayLeftAction>(TRAY_LEFT_DEFAULT);
  // 顶部 toast：成功 2.5s 自动消失，失败常驻
  const [toast, setToast] = useState<{ text: string; kind: "ok" | "error" } | null>(null);
  const toastTimer = useRef<number | null>(null);
  // 主题应用与跟随（设置页自身也随切换即时换色）
  useTheme();

  useEffect(() => {
    (async () => {
      try {
        const s = (await invoke("get_settings")) as Record<string, string>;
        if (s.glm_base) setGlmBase(s.glm_base);
        // 回显已存 Key（2026-09-17 所有者要求；未配置过则保持空，来源经 glm_token_source 徽标提示）
        if (s.glm_token) {
          setGlmToken(s.glm_token);
          savedTokenRef.current = s.glm_token;
          setCredMode("manual"); // 已存 Key = 手动指定模式（与后端"非空即应用设置"判定同源）
        }
        if (s.threshold_warn) setWarn(s.threshold_warn);
        if (s.threshold_danger) setDanger(s.threshold_danger);
        if (s.cleanup_days) {
          const days = Number(s.cleanup_days);
          // 旧档位（2 年 / 3 年 / 永不）已从选项移除，回落到默认 12 个月，避免下拉框空白
          if (CLEANUP_OPTIONS.some((o) => o.days === days)) setCleanupDays(days);
        }
        if (s.glm_token_source) setTokenFrom(s.glm_token_source);
        if (s.island_autohide !== undefined) setAutoHide(s.island_autohide !== "0");
        if (s.hover_expand !== undefined) setHoverCard(s.hover_expand !== "0");
        if (s.island_opacity !== undefined) setIslandOpacity(asIslandOpacity(s.island_opacity));
        if (s.dev_mode !== undefined) setDevMode(s.dev_mode === "1");
        setThemeMode(asThemeMode(s.theme));
        // 未设置/脏值一律回落默认「无操作」（与 Rust 端 _ => "none" 同源）
        if (s.tray_left_action === "none" || s.tray_left_action === "toggle" || s.tray_left_action === "menu") {
          setTrayLeft(s.tray_left_action);
        }
        if (s.agents_enabled) {
          try {
            const list = JSON.parse(s.agents_enabled) as string[];
            if (Array.isArray(list)) setAgents(list);
          } catch {
            /* 解析失败用默认全选 */
          }
        }
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
        await refreshHooksStatus();
        setAutoStart(await invoke("autostart_get"));
      } catch {
        /* 加载失败保持默认值 */
      }
    })();
    // 卸载时清掉未触发的 toast 与不透明度落库定时器
    return () => {
      if (toastTimer.current !== null) window.clearTimeout(toastTimer.current);
      if (opacitySaveTimer.current !== null) window.clearTimeout(opacitySaveTimer.current);
    };
  }, []);

  /** 顶部 toast：成功短暂提示后自动消失，失败常驻直到下一次提示覆盖 */
  const showToast = (text: string, kind: "ok" | "error") => {
    setToast({ text, kind });
    if (toastTimer.current !== null) window.clearTimeout(toastTimer.current);
    if (kind === "ok") {
      toastTimer.current = window.setTimeout(() => setToast(null), 2500);
    }
  };

  /** 单键落库；失败经 toast 提示（不阻塞界面）；返回是否成功，供调用方决定后续反馈 */
  const saveKey = async (key: string, value: string): Promise<boolean> => {
    try {
      await invoke("set_setting", { key, value });
      return true;
    } catch (e) {
      showToast(`保存失败：${e}`, "error");
      return false;
    }
  };

  /** 主题：点击即持久化并广播（广播含本窗口，useTheme 收到后即时切换 = 即时预览） */
  const changeTheme = async (mode: ThemeMode) => {
    setThemeMode(mode);
    await saveKey("theme", mode);
    await emit("theme-changed", mode).catch(() => {});
  };

  /** 托盘左键动作：即存即生效（托盘点击时 Rust 实时读库，无需广播） */
  const changeTrayLeft = async (v: TrayLeftAction) => {
    setTrayLeft(v);
    await saveKey("tray_left_action", v);
  };

  /** 贴边自动隐藏：即存即生效；关掉时若岛正处于隐藏态，Rust 会把它滑回显示 */
  const toggleAutoHide = async (v: boolean) => {
    setAutoHide(v);
    await saveKey("island_autohide", v ? "1" : "0");
    await invoke("island_refresh").catch(() => {});
  };

  /** 悬停展开：即存并实时推送给岛窗口 */
  const toggleHoverCard = async (v: boolean) => {
    setHoverCard(v);
    await saveKey("hover_expand", v ? "1" : "0");
    await emit("hover-expand-changed", v).catch(() => {});
  };

  /** 背景不透明度滑块：拖动中实时广播（岛即实时预览，无需预览控件），
   *  停手 300ms 后才落库，避免拖动过程高频写库 */
  const changeIslandOpacity = (v: number) => {
    setIslandOpacity(v);
    void emit(ISLAND_OPACITY_EVENT, v).catch(() => {});
    if (opacitySaveTimer.current !== null) window.clearTimeout(opacitySaveTimer.current);
    opacitySaveTimer.current = window.setTimeout(() => {
      void saveKey(ISLAND_OPACITY_KEY, String(v));
    }, 300);
  };

  /** 已勾选 Agent 的展示色（自定义 → 系统默认） */
  const effColor = (id: string) => agentColors[id] ?? AGENT_COLORS[id];

  /** 校验已勾选 Agent 间颜色不重复（颜色 = 身份）；通过则落库并推送岛窗口，撞色返回 false */
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
    savedColorsRef.current = colors;
    await saveKey("agent_colors", JSON.stringify(colors));
    await emit("agents-changed", { agents: list, colors }).catch(() => {});
    return true;
  };

  /** 勾选/取消 Agent：即存即生效；勾选集变化会改变撞色判定范围，顺带重新校验 */
  const toggleAgent = async (id: string, checked: boolean) => {
    const next = checked ? [...agents, id] : agents.filter((x) => x !== id);
    setAgents(next);
    await saveKey("agents_enabled", JSON.stringify(next));
    await commitColors(agentColors, next);
  };

  /** 颜色选择失焦 = 改完：撞色回滚本次改动并就近提示，通过则落库 */
  const onColorBlur = () => {
    commitColors(agentColors, agents).then((ok) => {
      if (!ok) setAgentColors({ ...savedColorsRef.current });
    });
  };

  /** 恢复全部 Agent 默认身份色 */
  const resetColors = async () => {
    setAgentColors({});
    await commitColors({}, agents);
    showToast("已恢复默认颜色", "ok");
  };

  /** 平台切换：即存；重启后聚合器按新平台查询（事实写入分区副标题，不弹提示） */
  const changeGlmBase = async (v: string) => {
    setGlmBase(v);
    await saveKey("glm_base", v);
  };

  /** API Key 失焦提交：与已存值一致则跳过（防误点失焦重复落库/提示）；
   *  手动模式下留空失焦 = 未修改（防误清空），要清除 Key 请切回"自动发现"模式。
   *  保存成功后输入框保留并回正内容（回显），徽标即时点亮 */
  const commitToken = async () => {
    const t = glmToken.trim();
    if (!t || t === savedTokenRef.current) return;
    if (await saveKey("glm_token", t)) {
      setGlmToken(t);
      savedTokenRef.current = t;
      // 重启后聚合器会把徽标改写为实际来源（应用设置）
      setTokenFrom("已保存（重启后生效）");
      showToast("已保存，凭据在重启应用后生效", "ok");
    }
  };

  /** 凭据来源切换（2026-09-18 模式化改造）：自动发现 / 手动指定显式二选一；
   *  切到"自动发现" = 显式清除已存 Key（写空值回落发现链），toast 告知避免静默切换；
   *  切到"手动指定"只改界面状态，等输入框失焦再落库 */
  const changeCredMode = async (mode: "auto" | "manual") => {
    if (mode === credMode) return;
    setCredMode(mode);
    if (mode === "auto") {
      setGlmToken("");
      savedTokenRef.current = "";
      if (await saveKey("glm_token", "")) {
        setTokenFrom("自动发现（重启后生效）");
        showToast("已切换为自动发现，重启应用后生效", "ok");
      } else {
        setCredMode("manual"); // 落库失败回滚界面，避免显示与实际不符
      }
    }
  };

  /** 阈值失焦校验并落库：两个值都合法且琥珀 < 红色才写入，否则就近提示 */
  const commitThresholds = async () => {
    const w = Number(warn);
    const d = Number(danger);
    const inRange = (v: number) => Number.isFinite(v) && v > 0 && v <= 100;
    if (!inRange(w) || !inRange(d)) {
      setThresholdError("阈值须为 1～100 的数字");
      return;
    }
    if (w >= d) {
      setThresholdError("琥珀阈值须小于红色阈值");
      return;
    }
    setThresholdError("");
    await saveKey("threshold_warn", warn);
    await saveKey("threshold_danger", danger);
  };

  /** Enter 直接提交（失焦提交的快捷路径） */
  const blurOnEnter = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter") (e.target as HTMLInputElement).blur();
  };

  /** hooks 注入/卸载（M2-6/7 起按 Agent 独立装卸）：本地文件操作，busy 态记录到各家防连点 */
  const toggleHooks = async (agent: string) => {
    setHookBusy((prev) => ({ ...prev, [agent]: true }));
    try {
      if (hooksOn[agent]) await invoke("uninstall_hooks", { agent });
      else await invoke("install_hooks", { agent });
      const on = await invoke<boolean>("hooks_status", { agent });
      setHooksOn((prev) => ({ ...prev, [agent]: on }));
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

  /** 开机自启：先切换 UI 再落命令，失败回滚并提示 */
  const toggleAutoStart = async (on: boolean) => {
    setAutoStart(on);
    try {
      await invoke("autostart_set", { enable: on });
    } catch (e) {
      setAutoStart(!on);
      showToast(`设置失败：${e}`, "error");
    }
  };

  /** 开发者模式：即存即生效（Rust 端 set_setting 命中后即时切换日志级别，免重启） */
  const toggleDevMode = async (v: boolean) => {
    setDevMode(v);
    await saveKey("dev_mode", v ? "1" : "0");
  };

  return (
    <div className="st-root">
      {toast && (
        <div
          className={`st-toast${toast.kind === "error" ? " st-toast-err" : ""}`}
          role="status"
        >
          {toast.text}
        </div>
      )}

      <Section title="通用" desc="应用主题与系统行为，改动即时生效">
        <Row title="主题" desc="跟随系统时随 Windows 深浅色自动切换（窗口标题栏颜色始终随系统）">
          <Segmented
            value={themeMode}
            onChange={changeTheme}
            ariaLabel="主题模式"
            options={[
              { value: "system", label: "跟随系统" },
              { value: "dark", label: "深色" },
              { value: "light", label: "浅色" },
            ]}
          />
        </Row>
        <Row title="开机自启" desc="登录 Windows 后自动启动并常驻托盘">
          <Switch checked={autoStart} onChange={toggleAutoStart} />
        </Row>
        <Row title="托盘左键" desc="单击托盘图标的动作；右键始终打开托盘菜单，双击不响应">
          <Segmented
            value={trayLeft}
            onChange={changeTrayLeft}
            ariaLabel="托盘左键动作"
            options={[
              { value: "none", label: "无操作" },
              { value: "toggle", label: "显隐灵动岛" },
              { value: "menu", label: "打开菜单" },
            ]}
          />
        </Row>
      </Section>

      <Section title="灵动岛" desc="拖到屏幕上 / 左 / 右边缘可自动贴靠">
        <Row
          title="贴边自动隐藏"
          desc="开启：贴靠边缘后滑出、仅露一点边缘，鼠标移入自动显示 / 关闭：贴边只吸附停靠，保持可见"
        >
          <Switch checked={autoHide} onChange={toggleAutoHide} />
        </Row>
        <Row
          title="悬停展开信息卡片"
          desc="开启：鼠标移入岛即展开卡片 / 关闭：点击展开、再点收回，移出后自动收起"
        >
          <Switch checked={hoverCard} onChange={toggleHoverCard} />
        </Row>
        <Row
          title="背景不透明度"
          desc="灵动岛胶囊、信息面板与贴边隐藏态的底色深浅；信息面板>胶囊>隐藏态"
        >
          <span className="st-slider-val">{islandOpacity}%</span>
          <input
            type="range"
            className="st-slider"
            min={ISLAND_OPACITY_MIN}
            max={ISLAND_OPACITY_MAX}
            step={1}
            value={islandOpacity}
            onChange={(e) => changeIslandOpacity(Number(e.target.value))}
            aria-label="背景不透明度"
          />
        </Row>
      </Section>

      <Section
        title="Agent 监控"
        desc="点击卡片启用或停用监控；「精确」注入 hooks 实时上报状态；色点决定岛分块与徽标的颜色"
      >
        {/* 卡片网格（2026-09-23 改版）：列数随窗口宽度自适应（1~4 列封顶，样式层控制） */}
        <div className="st-agents">
          {AGENT_DEFS.map((a) => {
            const checked = agents.includes(a.id);
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
              <div
                key={a.id}
                className={`st-agent${checked ? " st-agent-on" : ""}`}
                style={{ "--agent-color": effColor(a.id) } as React.CSSProperties}
                onClick={() => toggleAgent(a.id, !checked)}
                onKeyDown={(e) => {
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
                  {/* 自绘色点=取色入口：原生 color input 隐藏为点击代理（精致形态） */}
                  <span
                    className="st-agent-dot"
                    title={checked ? "自定义颜色" : "启用后可自定义颜色"}
                    onClick={(e) => e.stopPropagation()}
                  >
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
                  {a.label}
                  <span className="st-agent-switch" onClick={(e) => e.stopPropagation()}>
                    <Switch checked={checked} onChange={(v) => toggleAgent(a.id, v)} />
                  </span>
                </div>
                <div className="st-agent-status">
                  <span className="st-agent-status-text">{status}</span>
                  {hasHooks && (
                    <span
                      className={`st-agent-hooks${checked ? "" : " st-agent-hooks-off"}`}
                      title={
                        checked
                          ? "注入 hooks：Agent 实时上报状态（工作中/等待输入）；未注入时按进程启发式推断，约 90 秒精度。装卸均自动备份配置文件"
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
            );
          })}
        </div>
        {colorError && <div className="st-row-error">{colorError}</div>}
        <div className="st-agent-foot">
          <button type="button" className="st-btn st-btn-sm" onClick={resetColors}>
            恢复默认颜色
          </button>
        </div>
      </Section>

      <Section title="额度与凭据" desc="GLM Coding Plan 额度查询，凭据与阈值重启后生效">
        <Row title="平台" desc="额度接口所属站点">
          <select
            className="st-input st-input-sm"
            value={glmBase}
            onChange={(e) => changeGlmBase(e.target.value)}
          >
            <option value="https://open.bigmodel.cn">智谱 BigModel（国内）</option>
            <option value="https://api.z.ai">Z.AI（国际）</option>
          </select>
        </Row>
        <Row
          title="凭据来源"
          badge={tokenFrom ? <Badge on text={tokenFrom} /> : <Badge on={false} text="未发现" />}
          desc="自动发现顺序：环境变量 → claude-menu；手动指定优先生效，改动在重启应用后生效"
        >
          <Segmented
            value={credMode}
            onChange={changeCredMode}
            ariaLabel="凭据来源"
            options={[
              { value: "auto", label: "自动发现" },
              { value: "manual", label: "手动指定" },
            ]}
          />
        </Row>
        {/* API Key 行仅在手动指定模式下渲染：自动发现模式下 Key 由发现链提供，输入框无意义 */}
        {credMode === "manual" && (
          <Row
            title="API Key"
            desc="与 Claude Code 的 ANTHROPIC_AUTH_TOKEN 同值；留空失焦 = 未修改，清除 Key 请切回“自动发现”"
            tall
          >
            <div className="st-token">
              <input
                className="st-input"
                type={showToken ? "text" : "password"}
                value={glmToken}
                placeholder="失焦自动保存"
                onChange={(e) => setGlmToken(e.target.value)}
                onBlur={commitToken}
                onKeyDown={blurOnEnter}
              />
              <button
                type="button"
                className="st-eye"
                title={showToken ? "隐藏" : "显示"}
                onClick={() => setShowToken(!showToken)}
              >
                <EyeIcon off={!showToken} />
              </button>
            </div>
          </Row>
        )}
        <Row
          title="额度提醒阈值（%）"
          desc="已用额度达到琥珀阈值开始提醒，达到红色阈值转为告警，重启后生效"
          error={thresholdError}
        >
          <div className="st-th">
            <i className="st-dot st-dot-amber" />
            <input
              className="st-input st-input-num"
              type="number"
              min={1}
              max={100}
              value={warn}
              onChange={(e) => setWarn(e.target.value)}
              onBlur={commitThresholds}
              onKeyDown={blurOnEnter}
              aria-label="琥珀提醒阈值"
            />
          </div>
          <div className="st-th">
            <i className="st-dot st-dot-red" />
            <input
              className="st-input st-input-num"
              type="number"
              min={1}
              max={100}
              value={danger}
              onChange={(e) => setDanger(e.target.value)}
              onBlur={commitThresholds}
              onKeyDown={blurOnEnter}
              aria-label="红色告警阈值"
            />
          </div>
        </Row>
      </Section>

      <Section title="数据与维护" desc="历史数据清理与程序日志">
        <Row title="统计数据保留时长" desc="应用启动时按周期清理历史用量 / 快照 / 事件">
          <select
            className="st-input st-input-sm"
            value={cleanupDays}
            onChange={(e) => {
              const days = Number(e.target.value);
              setCleanupDays(days);
              saveKey("cleanup_days", String(days));
            }}
          >
            {CLEANUP_OPTIONS.map((o) => (
              <option key={o.days} value={o.days}>
                {o.days === CLEANUP_DEFAULT_DAYS ? `${o.label}（默认）` : o.label}
              </option>
            ))}
          </select>
        </Row>
        <Row
          title="开发者模式"
          desc="记录更详细的程序日志便于排障，正常使用无需开启"
        >
          <Switch checked={devMode} onChange={toggleDevMode} />
        </Row>
      </Section>
    </div>
  );
}
