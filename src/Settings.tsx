/**
 * 设置页（T11 / 2026-09-17 重构）：分区卡片 + 设置项即时生效
 * - 布局：统一"设置行"（固定标题 + 固定描述 + 右侧控件），按使用频率分节；
 *   分区头为主标题 + 副标题同行（副标题不换行，窗口最小宽度据此设下限）
 * - 交互：改动即存即生效（无保存按钮、不再保存后自动关窗）；开关行标题/描述固定，
 *   状态由 Switch 与徽标表达，文案不随选中态变化（遵循 Fluent 开关文案规范）
 * - 反馈：校验错误内联显示在出错行正下方 + 出错输入框边框标红（2026-09-24）；
 *   操作结果用顶部 toast（成功 2.5s 自动消失，失败常驻直到下一次提示）
 * - 界面文案一律简体中文标点（2026-09-17 验收建议 3）
 * - 2026-09-24 体验改造（L1~X2）：内容列限宽居中 + 分区头吸顶；分区重排为
 *   灵动岛 → Agent 监控 → 额度与凭据 → 通用 → 数据与维护（高频前置）；
 *   行描述精简为一句话，技术细节挪行尾 ⓘ（信息只挪不丢）
 * - 2026-09-28 阶段二（锚点条）：内容列顶部 sticky 分区导航——scroll-spy 高亮
 *   当前分区、点击平滑滚动直达；额度页「去设置添加」直达事件复用同一函数
 * - 2026-10-03 四轮审查拆分：本文件降为编排层（布局/导航/toast/灵动岛/通用/
 *   数据与维护分区），Agent 监控与额度与凭据两大状态簇各自拆为
 *   settings/AgentSection 与 settings/QuotaSection，弹窗拆为 settings/
 *   ProviderFormModal（表单状态挂载即初始化），基础件沉 settings/widgets——
 *   阈值/预算输入的击键与拖色不再触发全页 reconcile
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { asThemeMode, useTheme, type ThemeMode } from "./shared/theme";
import { invokeReady } from "./shared/invokeReady";
import {
  ISLAND_OPACITY_DEFAULT,
  ISLAND_OPACITY_EVENT,
  ISLAND_OPACITY_KEY,
  ISLAND_OPACITY_MAX,
  ISLAND_OPACITY_MIN,
  asIslandOpacity,
} from "./shared/islandOpacity";
import Toast, { type ToastData } from "./shared/Toast";
import AgentSection from "./settings/AgentSection";
import QuotaSection from "./settings/QuotaSection";
import OnboardingCard from "./settings/OnboardingCard";
import {
  blurOnEnter,
  CLEANUP_DEFAULT_DAYS,
  CLEANUP_OPTIONS,
  HOTKEY_ISLAND_DEFAULT,
  NAV_HEIGHT_PX,
  NAV_SECTIONS,
  QUOTA_SECTION_ID,
  Row,
  scrollToSection,
  Section,
  SectionNav,
  Segmented,
  Switch,
  type TrayLeftAction,
  TRAY_LEFT_DEFAULT,
} from "./settings/widgets";
import "./settings.css";

export default function Settings() {
  // —— 灵动岛分区 ——
  // 贴边自动隐藏（缺省=开，与 Rust 端 autohide_enabled 的默认一致）
  const [autoHide, setAutoHide] = useState(true);
  // 岛背景不透明度（缺省 80%，2026-10-10 所有者拍板；面板/隐藏态按偏移派生）
  const [islandOpacity, setIslandOpacity] = useState(ISLAND_OPACITY_DEFAULT);
  // 不透明度落库防抖定时器：拖动中只广播不写库，停手 300ms 后落一次盘
  const opacitySaveTimer = useRef<number | null>(null);
  // 上次成功落库的不透明度（五轮审查批次一）：落库失败时把岛端预览拉回已保存值，
  // 防「拖动预览了新值→保存失败→岛停留在未保存值直到重启」的假生效残留
  const savedOpacityRef = useRef(ISLAND_OPACITY_DEFAULT);
  // 悬停自动展开信息卡片（缺省=开；关闭时点击岛展开/收回）
  const [hoverCard, setHoverCard] = useState(true);
  // —— 通用分区 ——
  // 主题模式（跟随系统/深色/浅色，点击即切换全窗口预览）
  const [themeMode, setThemeMode] = useState<ThemeMode>("system");
  // 托盘左键动作（缺省=无操作，与 Rust 端 tray_left_action 的默认一致；右键恒为菜单）
  const [trayLeft, setTrayLeft] = useState<TrayLeftAction>(TRAY_LEFT_DEFAULT);
  // 全局快捷键（07-UX 2.3）：存组合串，""＝关闭，键缺失＝默认启用（Rust 端同默认）
  const [hotkeyIsland, setHotkeyIsland] = useState<string>(HOTKEY_ISLAND_DEFAULT);
  // 会话等待输入系统通知（07-UX 2.6，缺省=关——红线⑤ opt-in 本体）
  const [notifyWaiting, setNotifyWaiting] = useState(false);
  const [autoStart, setAutoStart] = useState(false);
  // 开发者模式（缺省=关；开启后 Rust 端即时切到 Debug 级日志，免重启）
  const [devMode, setDevMode] = useState(false);
  // —— 数据与维护分区 ——
  // 数据保留周期（未设置时后端按 1 年清理，前端默认值与之对齐）
  const [cleanupDays, setCleanupDays] = useState(CLEANUP_DEFAULT_DAYS);
  // 本地只读 API（#25）：开关与端口，变更重启生效
  const [localApi, setLocalApi] = useState(false);
  const [apiPort, setApiPort] = useState("6737");
  const [apiPortError, setApiPortError] = useState("");
  // —— 页面级 ——
  // 顶部 toast（共享 Toast 组件）：成功 2.5s 自动消失，失败常驻；计时在组件内
  const [toast, setToast] = useState<ToastData | null>(null);
  // 锚点条当前分区（scroll-spy，阶段二）：滚动中取「分区顶已越过锚点条底缘」的
  // 最后一家；页面滚到底时兜底为最后一家（尾部分区可能不够高滚不到顶）
  const [currentSection, setCurrentSection] = useState(NAV_SECTIONS[0].id);
  const pageRef = useRef<HTMLDivElement>(null);
  // 主题应用与跟随（设置页自身也随切换即时换色）；syncNative＝标题栏颜色随应用主题（07-UX 2.4）
  useTheme({ syncNative: true });

  // 自加载编排层各分区设置切片（Agent/额度分区的切片由各自组件拉取）
  useEffect(() => {
    (async () => {
      try {
        // 同页面的实例加载一样走重试：此前此处竞态失败被静默吞掉（设置项悄悄
        // 回落默认值），toast 暴露竞态后一并修正
        const s = await invokeReady<Record<string, string>>("get_settings");
        if (s.island_autohide !== undefined) setAutoHide(s.island_autohide !== "0");
        if (s.hover_expand !== undefined) setHoverCard(s.hover_expand !== "0");
        if (s.island_opacity !== undefined) {
          const v = asIslandOpacity(s.island_opacity);
          setIslandOpacity(v);
          savedOpacityRef.current = v;
        }
        if (s.dev_mode !== undefined) setDevMode(s.dev_mode === "1");
        setThemeMode(asThemeMode(s.theme));
        // 未设置/脏值一律回落默认「无操作」（与 Rust 端 _ => "none" 同源）
        if (s.tray_left_action === "none" || s.tray_left_action === "toggle" || s.tray_left_action === "menu") {
          setTrayLeft(s.tray_left_action);
        }
        // 全局快捷键：键缺失＝默认启用 Ctrl+Alt+I（Rust 端启动注册同默认）；
        // 键存在（含空串＝关闭）原样采用
        if (s.hotkey_toggle_island !== undefined) setHotkeyIsland(s.hotkey_toggle_island);
        if (s.notify_waiting !== undefined) setNotifyWaiting(s.notify_waiting === "1");
        if (s.cleanup_days) {
          const days = Number(s.cleanup_days);
          // 旧档位（2 年 / 3 年 / 永不）已从选项移除，回落到默认 12 个月，避免下拉框空白
          if (CLEANUP_OPTIONS.some((o) => o.days === days)) setCleanupDays(days);
        }
        if (s.local_api_enabled === "1") setLocalApi(true);
        if (s.local_api_port) {
          setApiPort(s.local_api_port);
          savedApiPortRef.current = s.local_api_port;
        }
        setAutoStart(await invoke<boolean>("autostart_get"));
      } catch {
        /* 加载失败保持默认值 */
      }
    })();
    // 卸载时清掉未触发的不透明度落库定时器（toast 计时随共享组件卸载自清）
    return () => {
      if (opacitySaveTimer.current !== null) window.clearTimeout(opacitySaveTimer.current);
    };
  }, []);

  /** 顶部 toast：数据摆给共享 Toast（成功 2.5s 自动消失／失败常驻的计时在组件内） */
  const showToast = (text: string, kind: "ok" | "error") => {
    setToast({ text, kind });
  };

  /** 单键落库；失败经 toast 提示（不阻塞界面）；返回是否成功，供调用方决定后续反馈 */
  const saveKey = useCallback(
    async (key: string, value: string): Promise<boolean> => {
      try {
        await invoke("set_setting", { key, value });
        return true;
      } catch (e) {
        showToast(`保存失败：${e}`, "error");
        return false;
      }
    },
    // showToast 每次渲染重建但仅事件里用——eslint 不在项目内，语义上此回调
    // 恒稳定，子分区 effect 不受影响
    [],
  );

  /** 主题：点击即持久化并广播（广播含本窗口，useTheme 收到后即时切换 = 即时预览）。
   *  保存失败不广播（四轮审查）：原先失败仍照发——全部窗口预览切到新主题而
   *  重启后回落旧值，「假成功」误导；现在仅本窗口预览＋toast 报错 */
  const changeTheme = async (mode: ThemeMode) => {
    setThemeMode(mode);
    if (!(await saveKey("theme", mode))) return;
    await emit("theme-changed", mode).catch(() => {});
  };

  /** 托盘左键动作：即存即生效（托盘点击时 Rust 实时读库，无需广播）。
   *  失败回滚显示（五轮审查批次一）：保存失败时分段控件跳回旧值，防开关位置
   *  与持久值脱节（重启后「设置自己变了」的错觉） */
  const changeTrayLeft = async (v: TrayLeftAction) => {
    const prev = trayLeft;
    setTrayLeft(v);
    if (!(await saveKey("tray_left_action", v))) setTrayLeft(prev);
  };

  /** 全局快捷键（07-UX 2.3）：Rust 侧先注册后落库，注册失败（组合被占用）
   *  返回 Err——saveKey 已 toast 报错，这里回滚分段控件显示（不假开） */
  const changeHotkeyIsland = async (v: string) => {
    const prev = hotkeyIsland;
    setHotkeyIsland(v);
    if (!(await saveKey("hotkey_toggle_island", v))) setHotkeyIsland(prev);
  };

  /** 会话等待通知（07-UX 2.6）：即存即生效（聚合器每 tick 现读），失败回滚 */
  const toggleNotifyWaiting = async (v: boolean) => {
    setNotifyWaiting(v);
    if (!(await saveKey("notify_waiting", v ? "1" : "0"))) setNotifyWaiting(!v);
  };

  /** 贴边自动隐藏：即存即生效；关掉时若岛正处于隐藏态，Rust 会把它滑回显示。
   *  失败回滚＋不再触发 island_refresh（五轮审查批次一，对齐 toggleAutoStart 模式） */
  const toggleAutoHide = async (v: boolean) => {
    setAutoHide(v);
    if (!(await saveKey("island_autohide", v ? "1" : "0"))) {
      setAutoHide(!v);
      return;
    }
    await invoke("island_refresh").catch(() => {});
  };

  /** 悬停展开：即存并实时推送给岛窗口。
   *  失败不广播＋回滚（五轮审查批次一）：原先失败仍照发——岛端立即切到新行为
   *  而重启后回落旧值，与 changeTheme「失败不广播」同一纪律（修点修面） */
  const toggleHoverCard = async (v: boolean) => {
    setHoverCard(v);
    if (!(await saveKey("hover_expand", v ? "1" : "0"))) {
      setHoverCard(!v);
      return;
    }
    await emit("hover-expand-changed", v).catch(() => {});
  };

  /** 背景不透明度滑块：拖动中实时广播（岛即实时预览，无需预览控件），
   *  停手 300ms 后才落库，避免拖动过程高频写库。
   *  落库失败把岛端拉回已保存值（五轮审查批次一）：拖动预览是过程态，
   *  失败后停留 = 假生效残留 */
  const changeIslandOpacity = (v: number) => {
    setIslandOpacity(v);
    void emit(ISLAND_OPACITY_EVENT, v).catch(() => {});
    if (opacitySaveTimer.current !== null) window.clearTimeout(opacitySaveTimer.current);
    opacitySaveTimer.current = window.setTimeout(() => {
      void saveKey(ISLAND_OPACITY_KEY, String(v)).then((ok) => {
        if (ok) savedOpacityRef.current = v;
        else void emit(ISLAND_OPACITY_EVENT, savedOpacityRef.current).catch(() => {});
      });
    }, 300);
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

  /** 开发者模式：即存即生效（Rust 端 set_setting 命中后即时切换日志级别，免重启）。
   *  失败回滚（五轮审查批次一）：Rust 没收到就不该显示已切换 */
  const toggleDevMode = async (v: boolean) => {
    setDevMode(v);
    if (!(await saveKey("dev_mode", v ? "1" : "0"))) setDevMode(!v);
  };

  const toggleLocalApi = async (v: boolean) => {
    setLocalApi(v);
    if (!(await saveKey("local_api_enabled", v ? "1" : "0"))) setLocalApi(!v);
  };
  // 上次成功落库的端口（失败回滚显示用，防「假保存」）
  const savedApiPortRef = useRef("6737");
  const commitApiPort = async () => {
    const p = Number(apiPort);
    if (!Number.isInteger(p) || p < 1 || p > 65535) {
      setApiPortError("端口须为 1–65535 的整数");
      return;
    }
    setApiPortError("");
    if (await saveKey("local_api_port", String(p))) savedApiPortRef.current = String(p);
    else setApiPort(savedApiPortRef.current); // 失败回滚输入框，重启前后显示一致
  };

  // 锚点条 scroll-spy（阶段二）：「分区顶已越过锚点条底缘（吸顶判定线）」的
  // 最后一家即当前分区；滚到底兜底最后一家（尾部分区可能不够高滚不到顶）。
  // 实现（2026-09-28 修复高亮卡死）：①监听挂 window 捕获段——scroll 事件不冒泡，
  // 捕获可截获 .st-page 的滚动且不依赖元素监听；②不走 rAF 合帧——设置窗口常驻
  // 隐藏，WebView 对隐藏窗口暂停 rAF，挂起的句柄不执行会把后续调度全部锁死
  // （高亮卡在首格的根因），直算 5 次 getBoundingClientRect 开销可忽略
  const computeSection = useCallback(() => {
    const page = pageRef.current;
    if (!page) return;
    let idx = 0;
    NAV_SECTIONS.forEach((s, i) => {
      const top = document.getElementById(s.id)?.getBoundingClientRect().top ?? Infinity;
      if (top <= NAV_HEIGHT_PX + 4) idx = i;
    });
    // 滚到底兜底；scrollHeight 为 0（极端无布局）时跳过，避免误判到末位
    if (page.scrollHeight > 0 && page.scrollTop + page.clientHeight >= page.scrollHeight - 2) {
      idx = NAV_SECTIONS.length - 1;
    }
    setCurrentSection(NAV_SECTIONS[idx].id);
  }, []);

  useEffect(() => {
    const onScroll = () => computeSection();
    // 第三参 true = 捕获段；移除时必须同样传 true（capture 标记参与匹配）
    window.addEventListener("scroll", onScroll, true);
    computeSection();
    return () => {
      window.removeEventListener("scroll", onScroll, true);
    };
  }, [computeSection]);

  // 额度页空态「去设置添加」直达：滚动定位到「额度与凭据」分区（M3-5）。
  // 设置窗口常驻隐藏（监听器一直存活），事件在 show 之后到达也能命中；
  // 隐藏期间滚动事件可能缺失，滚动后立即＋延迟各补算一次高亮
  useEffect(() => {
    const un = listen("goto-quota-section", () => {
      scrollToSection(QUOTA_SECTION_ID);
      computeSection();
      window.setTimeout(computeSection, 600); // 平滑滚动落定后校正
    });
    return () => {
      un.then((f) => f());
    };
  }, [computeSection]);

  /** 锚点跳转：平滑滚动到分区顶（减弱动态效果时瞬时定位） */
  const jumpToSection = (id: string) => {
    scrollToSection(id);
  };

  return (
    /* 外壳层承接全窗滚动与根背景（L1），内容列 .st-root 限宽居中；
       pageRef 供锚点条 scroll-spy 监听滚动 */
    <div className="st-page" ref={pageRef}>
      <div className="st-root">
        {/* 共享 toast（2026-10-09 提炼）：设置页定位变体让位吸顶锚点条 */}
        <Toast data={toast} onDismiss={() => setToast(null)} className="st-toast-top" />

        <SectionNav current={currentSection} onJump={jumpToSection} />

        {/* 首次引导三步上手卡（07-UX 1.1）：一次性，自包含（自查 onboarding_done
            决定渲染，数据自拉事件自刷新），完成后不再出现 */}
        <OnboardingCard />

        <Section title="灵动岛" id="st-section-island" desc="拖到屏幕上 / 左 / 右边缘可自动贴靠">
          <Row
            title="贴边自动隐藏"
            desc="贴靠边缘后自动滑出隐藏，移入鼠标即显示"
            tip="关闭后贴边仅吸附停靠，保持可见"
          >
            <Switch checked={autoHide} onChange={toggleAutoHide} />
          </Row>
          <Row
            title="悬停展开信息卡片"
            desc="鼠标移入岛即展开信息卡片"
            tip="关闭后改为点击展开、再点收回，移出后自动收起"
          >
            <Switch checked={hoverCard} onChange={toggleHoverCard} />
          </Row>
          <Row
            title="背景不透明度"
            desc="灵动岛胶囊与信息面板共用的底色深浅"
            tip="面板比胶囊实一档；贴边隐藏态为恒定近黑，不随滑块（Apple 式硬件物件）"
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

        <AgentSection saveKey={saveKey} showToast={showToast} />

        <QuotaSection saveKey={saveKey} showToast={showToast} />

        <Section title="通用" id="st-section-general" desc="应用主题与系统行为，改动即时生效">
          <Row
            title="主题"
            desc="跟随系统时随 Windows 深浅色自动切换"
            tip="标题栏颜色随应用主题同步；跟随系统档由系统原生跟随"
          >
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
          <Row
            title="托盘左键"
            desc="单击托盘图标的动作"
            tip="右键始终打开托盘菜单，双击不响应"
          >
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
          {/* 全局快捷键（07-UX 2.3）：默认启用 Ctrl+Alt+I（键缺失＝启用，Rust 端
              启动注册同默认）；组合被占用时 Rust 注册失败报错——toast 提示且
              分段控件回滚，不假开 */}
          <Row
            title="全局快捷键"
            desc="按下 Ctrl+Alt+I 在显示／隐藏灵动岛间切换"
            tip="全局生效，但只切换岛自身的显隐——不弹窗不抢焦点；组合被其他程序占用时无法启用"
          >
            <Segmented
              value={hotkeyIsland}
              onChange={changeHotkeyIsland}
              ariaLabel="全局快捷键"
              options={[
                { value: "", label: "关闭" },
                { value: HOTKEY_ISLAND_DEFAULT, label: "Ctrl+Alt+I" },
              ]}
            />
          </Row>
          {/* 会话等待通知（07-UX 2.6，默认关——红线⑤ opt-in 本体）：聚合器检测
              「进入等待」边沿经系统 toast 提醒，全屏干活不再错过卡住的 Agent */}
          <Row
            title="会话等待通知"
            desc="Agent 会话等待输入或批准时发系统通知，默认关闭"
            tip="同一会话 30 分钟内只提醒一次；通知行为遵循系统通知设置（勿扰/焦点辅助下可能被系统延后或收纳）"
          >
            <Switch checked={notifyWaiting} onChange={toggleNotifyWaiting} />
          </Row>
        </Section>

        <Section title="数据与维护" id="st-section-data" desc="历史数据清理、数据接口与程序日志">
          <Row
            title="统计数据保留时长"
            desc="应用启动与每日按周期清理历史用量 / 快照 / 事件"
          >
            <select
              className="st-input st-input-sm"
              value={cleanupDays}
              onChange={(e) => {
                const days = Number(e.target.value);
                const prev = cleanupDays;
                setCleanupDays(days);
                // 失败回滚下拉显示（五轮审查批次一）：防选了新档而库还是旧档
                void saveKey("cleanup_days", String(days)).then((ok) => {
                  if (!ok) setCleanupDays(prev);
                });
              }}
            >
              {CLEANUP_OPTIONS.map((o) => (
                <option key={o.days} value={o.days}>
                  {o.days === CLEANUP_DEFAULT_DAYS ? `${o.label}（默认）` : o.label}
                </option>
              ))}
            </select>
          </Row>
          {/* 本地只读 API（#25）：供脚本/小组件读取观测数据；仅回环只读、
              默认关（开端口也是打扰，红线⑤ opt-in）；变更重启生效 */}
          <Row
            title="本地只读 API"
            desc="在 127.0.0.1 暴露 /v1/status、/v1/limits、/v1/spend 只读 JSON，供脚本与小组件读取；默认关闭"
            tip="仅绑定本机回环地址、仅 GET、不含任何对话内容与凭据；开关与端口修改后重启应用生效"
            error={apiPortError || undefined}
          >
            <div className="st-th">
              <Switch checked={localApi} onChange={toggleLocalApi} />
              <span className="st-bal-sym">端口</span>
              <input
                className={`st-input st-input-num${apiPortError ? " st-input-err" : ""}`}
                disabled={!localApi}
                type="number"
                min={1}
                max={65535}
                value={apiPort}
                onChange={(e) => {
                  setApiPort(e.target.value);
                  setApiPortError("");
                }}
                onBlur={commitApiPort}
                onKeyDown={blurOnEnter}
                aria-label="本地 API 端口"
              />
            </div>
          </Row>

          <Row
            title="开发者模式"
            desc="记录更详细的程序日志便于排障，正常使用无需开启"
          >
            <Switch checked={devMode} onChange={toggleDevMode} />
          </Row>

          {/* 排障动线（07-UX 2.5）：日志与数据库落在 %APPDATA% 深处，两枚文字
              按钮直达真实目录（远程排障时不必再口述路径）；打开失败 toast 报错
              不静默。托盘菜单不加入口（所有者拍板：菜单只承载高频动作） */}
          <Row
            title="打开日志／数据文件夹"
            desc="直达程序日志与本地数据库所在目录，便于排障与备份"
          >
            <div className="st-th">
              <button
                type="button"
                className="st-btn st-btn-sm"
                onClick={() =>
                  invoke("open_app_dir", { kind: "log" }).catch((e) =>
                    showToast(`打开失败：${e}`, "error"),
                  )
                }
              >
                打开日志文件夹
              </button>
              <button
                type="button"
                className="st-btn st-btn-sm"
                onClick={() =>
                  invoke("open_app_dir", { kind: "data" }).catch((e) =>
                    showToast(`打开失败：${e}`, "error"),
                  )
                }
              >
                打开数据文件夹
              </button>
            </div>
          </Row>
        </Section>
      </div>
    </div>
  );
}
