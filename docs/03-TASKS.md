# AgentTrackerIsland 任务看板（03-TASKS）

> 阶段 3 执行清单 | 创建：2026-09-16 | 配套：[02-DESIGN](02-DESIGN.md)
> 规则（见 [WORKFLOW](WORKFLOW.md)）：每任务完成即更新本看板状态与 [HANDOFF](HANDOFF.md)；
> 做不完的任务如实记录进度后再结束会话；禁止看板外扩 scope；
> **构建打包仅在所有者明确宣布正式对外发布时执行**（见 WORKFLOW 构建打包纪律）

## 状态图例

⬜ 待办 | 🟨 进行中 | ✅ 完成（附日期/验收证据） | ⛔ 阻塞（附原因）

## M0 任务序列

### T0 环境准备 ✅（2026-09-16 完成）

- 内容：安装 Rust(rustup,stable-msvc，`--profile minimal`)+ VS Build Tools（MSVC C++ 工作负载）；
  目录按所有者 D 盘布局：`D:\Rust\{rustup,cargo}`(RUSTUP_HOME/CARGO_HOME)+ `D:\VSBuildTools`
- 验收：✅ rustc 1.98.1 + cargo 1.98.1（D:/Rust，PATH 已含 D:\Rust\cargo\bin）；
  ✅ cl.exe 在 D:\VSBuildTools\VC\Tools\MSVC\14.44.35207（vs_BuildTools.exe 直装，winget 本机不可用）
- 备注：安装细节与坑见 [HANDOFF](HANDOFF.md)

### T1 项目骨架 ✅（2026-09-16 完成）

- 内容：`F:\MyProjectRepository\AgentTrackerIsland`（更名前为 F:\AgentTracker，见 HANDOFF）下初始化 Tauri 2 + React/TS 模板，按 02-DESIGN §1 建目录
  （src-tauri/src/{collector,provider,state,store} + src/{island,shared}）；补充 .gitignore、git init
- 验收：✅ `tauri dev` 打开窗口（PID 2880 实测，内存 ~42MB）；✅ `cargo build` 全量通过（6m23s）
  + 前端 `npm run build` 通过；debug 产物 target/debug/agenttrackerisland.exe 生成
- 备注：模块子目录（collector/provider 等）尚未创建——T2 起按需建立；
  模块子目录随对应任务落地（避免空目录）
- 依赖：T0 ✅

### T2 存储层 ✅（2026-09-16 完成）

- 内容：rusqlite(bundled)+ migrations/0001_init.sql（02-DESIGN §3 全表）+ 增删查封装 + 水位管理
- 验收：✅ 单测 4/4 通过（迁移幂等/用量去重/水位单调/滚动清理，0.06s）；
  依赖 rusqlite 0.40.2 + anyhow（错误统一，后续适配器复用）
- 依赖：T1 ✅

### T3 ZCode 适配器 ✅（2026-09-16 完成）

- 内容：zcode.rs——只读打开 `~\.zcode\cli\db\db.sqlite`，列白名单读取 model_usage，
  水位增量采集入库，会话元数据 join session 表，状态启发式数据（last_usage_at）
- 验收：✅ 集成测试 2/2（连本机真实库）：全量采集 582 行入库幂等、水位增量正确、
  字段断言通过（毫秒时间戳/模型/agent）；采集全程 ZCode 运行中（本会话即 ZCode）零影响
- 备注：provider 由模型名前缀启发式映射（ZCode 的 provider_id 是内部 UUID）；
  历史数据存在大小写混用（GLM-5.3/glm-5.3），比较一律 to_ascii_lowercase
- 依赖：T2 ✅

### T4 Claude Code 适配器 ✅（2026-09-16 完成）

- 内容：claude_code.rs——扫描 `~\.claude\projects\**\*.jsonl`，解析 assistant usage，
  messageId（+requestId）去重保留最大快照，文件 mtime 过滤 + 时间水位增量，synthetic 行过滤
- 验收：✅ 单测 5/5 + 集成 4/4 全绿；
  ✅ **A2 对账完成**（详见 [01-RESEARCH §8](01-RESEARCH.md)）：516 条消息/27.98M tokens，
  与 ccusage 差 1.7%，差异来源已书面定位（ccusage 额外纳入 cost-state 非助手行源；
  本机数据无 requestId，去重口径与 ccusage/better-ccusage 完全同款）
- 里程碑：🎯 数据正确性关口通过，可继续 T5+
- 依赖：T2 ✅

### T5 GLM Provider ✅（2026-09-16 完成）

- 内容：provider/{mod,glm}.rs——ProviderAdapter trait、Monitor API 调用（5s 超时）、
  实测响应解析（TOKENS_LIMIT 区分 5h/weekly、percentage=已用%、重置毫秒时间戳）、
  凭据发现链（设置→环境变量→~\.claude\suppliers.json 自动发现）
- 验收：✅ 单测 7/7 + 真实调用通过：**凭据自动从 claude-menu 配置发现**；
  实测 [5h] 已用 100%（本会话消耗中）/ [weekly] 54%，重置时间正确解析；
  API 即官网数据源，百分比与官网必然一致（所有者可随时官网复核）
- 备注：① 定时刷新/失败降级调度在 T7 聚合器统一实现（后台线程所在层）；
  ② ⚠️ chrono 显示的是 UTC——UI 层倒计时/时间须转 Asia/Shanghai 本地时区
- 依赖：T2 ✅

### T6 hooks 链路 ✅（2026-09-16 完成）

- 内容：①诊断 hook 实测 stdin 字段（公共：session_id/transcript_path/cwd/hook_event_name；
  专有：tool_name/message/prompt_id 等）；②hook-bridge.js（白名单提取→事件文件 append，
  2s 兜底退出）；③Rust 事件消费者（字节偏移增量读，半行安全）；④install/uninstall_hooks
  （桥脚本编译进二进制，settings.json 备份+合并注入+防重复+原子写）
- 验收：✅ 单测 9/9；✅ 端到端实测：安装→`claude -p` 触发→**捕获 3 个真实事件**
  （SessionStart/UserPromptSubmit/SessionEnd）→消费→卸载后 settings 与原始状态语义等价；
  Drop 守卫保证测试 panic 不留注入残留
- 备注：①stdin 无 model 字段，02-DESIGN §4 事件协议已去 model（模型信息由 T4 补）；
  ②Windows 下 Rust 调 npm shim 需 `cmd /c`（不走 PATHEXT）；③事件协议含 message 字段
  （Notification 的限流关键词判定，T7 用）；④"10s 内反映到岛状态"在 T7/T9 联动验收
- 依赖：T2 ✅

### T7 状态聚合器 ✅（2026-09-16 完成）

- 内容：state/{mod,service}.rs——状态机纯函数（error 判定：限流正则/ZCode error_type/额度 100%；
  hooks 事件驱动；看门狗 5min；启发式 90s 活跃窗；进程枚举兜底 sysinfo）+ Aggregator 服务
  （10s tick：双适配器增量采集→入库→融合→快照；GLM 5min 刷新+失败降级最近快照；
  5h 额度 100% 时活跃会话标红）
- 验收：✅ 单测 13/13（状态全矩阵/看门狗/聚合优先级）；✅ e2e 本机真实数据：
  46 会话融合，岛 AnyError（GLM 5h 100% 正确触发），ZCode 本会话 47M tokens 统计正确；
  **当前 hooks 未安装（增强档已卸），状态来自启发式层——"拔掉 hooks 仍可用"已实测证明**
- 备注：设计缺陷被单测逮住并修正——hooks 事件有效窗口须大于看门狗窗口（30min vs 5min），
  否则看门狗分支不可达；quota 100% 的会话标红属产品口径，后续可在设置里提供开关
- 依赖：T3，T4，T5，T6 ✅

### T8 岛 UI-壳 ✅（2026-09-16 完成）

- 内容：tauri.conf.json 岛窗口（decorations:false/alwaysOnTop/skipTaskbar/transparent/
  resizable:false/shadow:false）+ 顶部居中定位与拖拽坐标记忆（Moved→app_settings）+
  托盘（显示/隐藏/退出）+ 后台聚合线程（10s tick→emit island-snapshot）+ 前端收缩态
  （胶囊/状态灯呼吸脉冲/摘要文案/token 缩写，CSS 自绘背景）
- 验收：✅ 形态经所有者屏幕实测确认；进程内存 39–48MB（含聚合器，红线 ≤100MB）；
  数据库落盘 %APPDATA%\com.agenttrackerisland.app\agenttrackerisland.db（2026-09-17 更名后路径），10s 刷新闭环
- 备注：① Acrylic 弃用——window-vibrancy 是**窗口级**效果，把整个矩形染灰破坏胶囊
  形态；正确做法=窗口全透明+CSS 自绘（依赖保留，M1 全宽形态可再评估）；
  ② 拖拽位置记忆与托盘菜单的交互行为并入 T12 冒烟一并验收
- 依赖：T7 ✅

### T9 岛 UI-内容 ✅（2026-09-16 完成）

- 内容：src/{shared/types.ts,island/{IslandBar,Panel}.tsx，App.tsx，App.css}——
  hover 展开（窗口高度 48→520 动态调整，宽恒定避免锚点跳变，不抢焦点）；
  会话卡片区（活跃优先排序/徽标 CC·ZC/模型/项目/token/状态，≤30 条滚动）；
  GLM 额度区（5h/周双进度条+百分比+倒计时本地 30s 推进）；
  静默提醒变色（≥80% 琥珀/≥95% 红，收缩态与进度条同步变色）
- 验收：✅ 所有者屏幕实测确认（展开/收起/卡片/额度条/变色）；
  数据 10s 刷新闭环（T8 已验）；capabilities 增 set-size/set-position 权限
- 依赖：T7，T8 ✅

### T10 点击跳转 🟨（2026-09-16 部分完成，所有者决定不阻塞）

- 内容：commands.rs——窗口枚举（EnumWindows+PID）+ 四级匹配（标题含完整路径 >
  目录名 > 进程链匹配（跑 claude 的 pwsh→父链→WT 宿主窗口）> Agent 关键词）+
  focus_session command；前端卡片 onClick
- 验收：✅ **A4 部分通过**：ZCode 卡片 → ZCode 桌面窗口激活正常；
  ❌ Windows Terminal（PowerShell 7 + Claude CLI）未命中——所有者指示搁置，
  不阻塞 M0（详见待议区）
- 依赖：T9 ✅

### T11 设置页 ✅（2026-09-16 完成）

- 内容：settings 窗口(#settings hash 分流)+ Settings.tsx 五区块（GLM 平台/key 密码框、
  提醒阈值、清理周期 8 档、hooks 启用/停用、开机自启 tauri-plugin-autostart）；
  Rust commands(get_settings/set_setting/hooks_*/autostart_*)；GLM 凭据优先级改
  应用设置>env>claude-menu；启动时按周期执行数据清理；设置窗口关闭即隐藏（可反复唤起）
- 验收：✅ 所有者实测通过（五区块正常/设置窗口反复唤起修复）；
  key 仅存本地 app_settings 不入日志；凭据/阈值重启生效已明示
- 依赖：T8 ✅（hooks 命令复用 T6）

### T12 验收与打包 ⏸（构建打包=正式发布动作：仅当所有者明确宣布对外发布时执行，见 WORKFLOW 构建打包纪律）

- 内容：红线回归（A6：退出/卸载工具后两 Agent 无报错）+ 性能（A5：内存≤100MB、冷启动≤2s）+
  便携 zip（release 产物+首次运行说明）
- ✅ 前置已完成：bundle active=false；代码细节调整 12 项（2026-09-16，提交 b0a1652）；
  UI 改动所有者已屏幕验收（累计前缀/阈值变色/保存自动关闭/滚动条）
- 📌 拍板回写（2026-09-17）：托盘"暂停监控"、额度手动刷新、error 原因展示三项均不做，
  已回写 02-DESIGN 相应小节
- 待办（dev 即可验，所有者）：①A1 waiting 场景——设置页启用 hooks，让 CC 等待输入，
  岛应变琥珀；②拖拽岛→重启→位置还原；③托盘三菜单项各验一遍
- 待办（随正式发布执行）：release 构建 + A5 性能实测 + A6 红线回归 + 便携 zip
  （触发条件=所有者明确宣布打包发布，平时一律不执行）
- 依赖：T3–T11（T10 部分完成已获所有者接受）

### T13 Dogfood 周 ⬜

- 内容：所有者日常自用 ≥1 周，问题记录到看板"待议区"
- 验收：**A7**——期间不再手动查官网额度；汇总问题清单定 M1 输入
- 依赖：T12

## M1 任务序列（2026-09-17 启动，优先级已与所有者确认）

> 顺序：P1→P6 逐项开发；构建打包不在此列（WORKFLOW 构建打包纪律）；
> 每项开工前先调研（不造轮子：🟢直接依赖/🟡借鉴实现/🔴自研）

### M1-1 报表页 🟨（P1，代码完成+自验通过，待所有者过目）

- 内容：独立窗口(#report 路由，与设置页同模式)；趋势图（按日 token）、热力图（周×小时
  用量分布）、按模型/供应商聚合占比、时间范围切换（7/30/90 天/全部）；
  数据源=自库 usage_records；入口=托盘菜单"报表…"；ECharts 按需引入
- ✅ 代码完成（2026-09-17）：store 四个聚合查询（report_daily/by_model/by_provider/heatmap，
  日界/星期/小时用 SQLite 'localtime' 取本机时区）+ 单测 test_report_aggregates；
  托盘"报表…"入口 + 关窗即隐藏（与设置页同模式）；前端 lazy 分割——echarts 独立
  chunk 仅报表窗口加载，岛主包只 +2KB；调研结论见 01-RESEARCH §9
- ✅ AgentTrackerIsland 实例内屏幕自验（2026-09-17）：四图+范围切换+滚动全正常，真实数据
  （09-16/09-17 单日 ~190M）；发现并修复模型名大小写切片问题（report_slice 按小写归一）
- 验收：所有者过目确认
- 依赖：无

### M1-2 搁置问题统一优化 ⬜（P2）

- 内容：所有者暂记的待优化清单（**待所有者补录**到本看板待议区），逐项修复
- 依赖：所有者提供清单

### M1-3 Codex CLI 接入 🟨（P3，源码级调研完成，适配器待实测条件）

- 内容：第一步先调研其本地数据格式（转录/事件/token 记录），勘察结论回填 01-RESEARCH；
  适配器实现待所有者实际使用 Codex 后进行（无真实数据不可验证）
- ✅ 调研完成（2026-09-17，源码级）：sessions/*.jsonl（rollout 格式）+ TokenCount 事件
  （input/cached/cache_write/output/reasoning/total），详见 01-RESEARCH §10；
  ⚠️ 新版出现 SQLite 状态库，JSONL 与状态库的读取面取舍待真实样本实测
- 待办：所有者安装并使用 Codex → 取真实 JSONL 样本实测字段 → 实现 AgentAdapter
- 依赖：M1-1；所有者环境

### M1-4 深浅双主题 ✅（2026-09-17 所有者验收通过）

- 内容：跟随系统+设置页自选；岛/面板/设置页/报表页色板变量化统一
- ✅ 代码完成（2026-09-17，纯前端 8 处，**零 Rust 改动、零新增权限**）：
  - 新增 `src/shared/theme.ts`：ThemeMode（system/dark/light，存 app_settings `theme` 键，
    默认 system）+ useTheme Hook（启动读设置→写 `<html>` data-theme；监听设置页
    theme-changed 广播；system 模式订阅 matchMedia 变化）
  - 跟随系统信号链（已核对本地 wry 0.55/tauri-runtime-wry 2.11 源码）：系统主题切换→
    tao ThemeChanged→运行时 SetPreferredColorScheme 推进 WebView2→matchMedia 触发，
    前端全程可感知，无需 Rust 参与
  - App.css：`:root` 暗色默认（=历史值，初始无 data-theme 不闪变）+ `[data-theme="light"]`
    浅色覆盖；23 个语义变量（文本四级/半透明面层/边框/悬停/输入框/滚动条/按钮/轨道等），
    settings.css/report.css 全部改引变量；状态色（绿/琥珀/红）与 Agent 身份色双主题通用不变
  - Settings.tsx：新增"外观"区块（跟随系统（默认）/深色/浅色）；保存写 theme 键 +
    emit theme-changed，三窗口（岛/设置/报表）即时生效无需重启
  - Report.tsx：ECharts 轴文字/网格线/饼图标签随主题（useTheme）；序列高饱和配色双主题共用
- 已知限制（有意为之，后续按需）：settings/report 窗口原生标题栏颜色始终跟随系统，
  不随应用内主题选择（内容区跟随）；如需标题栏联动须加 set-theme 权限，暂不做
- 验证：npm run build 通过（echarts chunk 警告为 M1-1 已知现状）；cargo test 19/19 绿
  （Rust 零改动命中缓存）；默认深色胶囊已在 dev 实例实屏确认与历史视觉一致；
  浅色切换/跟随系统/报表图表配色**已由所有者验收通过（2026-09-17）**
- 审查与优化（2026-09-17）：全量审查未发现阻塞缺陷；修复浅色下贴边标签 hover 无反馈
  （brightness 提亮对白色被钳制，补 `[data-theme="light"]` 悬停压暗 0.92，build 通过）；
  已知取舍：启动首帧闪变（浅色系统+system/浅色档，几十毫秒，不做）；暗色两处 α 微差
  （0.08→0.09、0.2→0.14，并入通用变量）；ECharts tooltip 白底为 M1-1 现状
- 依赖：M1-1 ✅

### M1-5 悬浮球/任务栏形态 🚫（2026-09-17 所有者拍板：划出不做了）

- **取消原因（所有者）**：灵动岛已支持自由拖拽 + 上/左/右三边贴靠 + 贴边自动隐藏
  （M1-6 验收通过），桌面悬浮球的核心价值（自由摆放/不占屏幕/随取随用）已被覆盖大半，
  不再单独开发；任务栏形态不再单列，归 M2 愿景池（如有需要再议）
- 决策记录：00-REQUIREMENTS 变更记录 v1.3

### M1-6 灵动岛贴边自动隐藏 🟨（追加需求 2026-09-17，代码完成待所有者屏幕验收）

- 内容（所有者追加，有望替代悬浮球）：岛可自由拖拽到屏幕任意位置；拖放到上/左/右边缘
  24px 内自动吸附；吸附后滑出屏外仅露 6px 边缘，鼠标移入滑入显示；设置页"灵动岛"区块
  控制开关，默认开启
- 实现：lib.rs IslandMotion 状态机（拖拽防抖看护线程：180ms 静默 + GetAsyncKeyState
  左键检测）+ 程序化滑动动画（代数守卫 + animating/programmed 双标记防自触发循环）+
  island_peek/island_refresh/island_drag_start/island_metrics commands + island-dock 前端事件；
  吸附优先级 上>左>右，几何纯函数 detect_edge/hidden_pos 有单测（18/18 绿）
- 迭代记录：①首测暴露拖拽权限缺失（core:window:default 不含 start-dragging，历史遗留）；
  ②隐藏几何改固定常量+逻辑坐标系（DPI 缩放适配，200% 屏实测）；③贴边隐藏态重设计：
  颜色=Agent 身份（用户可配，重复颜色保存拦截），等宽分段，状态用亮度/动效表达，
  左右为圆心辐射扇形+沿外沿额度弧线（SVG 同曲线描边），顶部含 Agent 标识+底边额度
  发丝线；④岛宽自适应：显示器逻辑宽×30% 夹取 [380,800]，island_metrics 同源；
  ⑤额度耗尽不再改写会话状态（quota_exhausted 快照标志），扇形"全红不渲染"根因即此；
  ⑥设置页新增：悬停/点击展开开关、监控 Agent 选择与颜色自定义（勾选才采集）；
  ⑦修复拖拽权限缺失、隐藏标签错位竞态、SVG 不渲染等（详见 HANDOFF 踩坑）
- 验收：所有者 dev 实测——自由拖拽 / 三边贴靠滑出 / 标签形状与位置 / 设置开关即时生效 /
  不同分辨率宽度自适应
- 备注：悬浮球形态已因本任务划出（M1-5 🚫,2026-09-17 所有者拍板）

### M1-7 审查修复与代码↔文档一致性回写 ✅（2026-09-17 完成）

- 触发：全量需求↔代码审查（2026-09-17）发现 3 项功能缺陷 + 多处文档未回写，
  所有者指示"先修复代码问题，再按代码优化文档"
- 修复内容：
  - ① **GLM 凭据回退（P1）**：设置 token 留空时自动回落发现链（env → claude-menu），
    兑现"留空则继续沿用"；设置页 Key 不回显、留空保存不覆盖已存 Key；
    凭据来源由聚合器启动时写入 `glm_token_source` 供设置页展示（service.rs/Settings.tsx）
  - ② **用量快照取最大（P2 同键部分）**：幂等键冲突时仅当新行四项合计更大才整行覆盖，
    与 CC 流式去重口径一致；跨 tick 重采到更完整快照可原地升级（store/mod.rs，
    新增单测 test_usage_upsert_keeps_max_snapshot）
  - ③ **进程探测误判（P3）**：hook-bridge 的 node 进程（路径含 .claude，寿命 ≤2s）
    不再被判定为 claude 存活（service.rs）
  - ④ calc_percent 不再拿 currentValue 绝对量冒充百分比，不可换算返回 None(glm.rs)
  - ⑤ 移除状态机 online 死状态（无产出路径；Rust 枚举/前端类型/排序/严重度/CSS 六处同步）
  - ⑥ 修正陈旧注释：App.tsx 岛宽 25%→30%、filterSnap 采集口径、lib.rs saved_pos
    物理→逻辑坐标、state/mod.rs 额度耗尽回写说明
- 文档回写：02-DESIGN §1 技术栈（edition/React/notify-rs/Acrylic/分发）、§2.1 trait
  实际签名+watch 备注、§2.2 凭据链与百分比兜底、§2.3 状态机五态+waiting 聚合+
  quota_exhausted、§3 清理范围、§4 hook 协议实测版、§5 托盘四菜单——与代码一致
- 验证：cargo test **19/19** 通过（新增 1 测）、npm run build 通过、cargo check 零警告
- 依赖：无

### M1-8 边角细节优化批次 ✅（2026-09-18 代码完成，待所有者 dev 验收）

- 触发：所有者委托"边边角角小细节优化改造"（托盘菜单/托盘图标尺寸/审查发现项），
  按审查清单分优先级拍板后实施
- 🐛 修复：①**托盘"显示/隐藏灵动岛"与贴边隐藏态互通**——贴边滑出时窗口仍在屏外
  "可见"，原先 hide/show 语义失效（显示只回屏外隐藏位、隐藏连边缘标签都藏掉）；
  现恢复走 peek_apply 滑回停靠位（lib.rs toggle_island）；②胶囊"额度 --/额度未配置"
  两处 tooltip 的字面 `\n`（JSX 字符串属性不解释转义，IslandBar.tsx）
- 🔧 托盘菜单：退出项前加分隔线；"灵动岛"项文案动态化（显示灵动岛/隐藏灵动岛，
  随贴边与窗口可见性刷新，TrayToggle 句柄入 managed state）；tooltip 更名
  "去你的岛（AgentTrackerIsland）"；三个辅助窗口菜单分支去重（show_aux_window）
- 🧹 卫生：移除未用依赖 window-vibrancy 0.8.0（M1-5 划出后保留理由失效；
  锁文件中 0.6.0 为 tauri 自身传递依赖，不受影响）；删模板残留 src/assets/react.svg；
  修 lib.rs saved_pos 陈旧注释（物理→逻辑坐标）
- ✨ 定名对齐（纯展示文案，未动 productName/identifier 与数据目录）：岛启动文案
  "「去你的岛」启动中…"；窗口标题"去你的岛 · 设置/报表/关于"；index.html title；README 状态徽章
- ⏸ 暂不处理（所有者拍板）：托盘图标尺寸——现状=ico 首条目 32×32，200% 屏 1:1
  完美，其他 DPI 由系统缩放；如需清晰化方案见 02-DESIGN §5 M1-8 回写
- 验证：cargo test **29/29** 通过（8 个集成测试按设计 ignore）、cargo check 零警告、
  npm run build 通过（echarts chunk 警告为 M1-1 已知现状）
- 🐛 **追加修复（2026-09-18 首验翻车，两轮定位）**：贴边隐藏态点托盘"显示灵动岛"
  后贴边区变黑、托盘菜单不再弹出、进程假死（主线程死锁，后台聚合线程仍存活，日志可证）。
  **真因（第二轮定位）**：peek_apply/apply_snap 尾部的 `let m = motion.lock()` 守卫
  实现 Drop 活到函数末尾，持锁调用 refresh_toggle_text（内部再次 lock motion），
  非重入互斥量同线程自锁——motion 被永久持有，滑动动画的 Moved 事件让主线程在
  事件处理器拿锁处永久阻塞（悬停滑入路径同样中招）。修复：先取值放锁再调
  refresh_toggle_text。第一轮误判为"菜单回调上下文与合成管线互等"，转线程修复
  无效但作为主线程减负的卫生习惯保留；线程约束相关表述已在文档更正
- 🔧 **托盘显隐语义重构（2026-09-18 二轮，所有者确认后实施）**：①菜单项去"…"
  （报表/设置/关于）；②显隐项改名"显示/隐藏"，文案纯按窗口可见性判定（胶囊态与
  贴边标签态都算可见，仅整窗 hide 后显示"显示"）；③"隐藏"=任意可见态一键彻底
  消失（游戏清屏场景）；④"显示"=临时召唤——先亮胶囊提醒位置（贴边标签太小
  用户未必记得），停靠边缘且自动隐藏开启则 3s 自动滑出收回（Rust 发 island-summon
  事件，前端定时器执行；鼠标移入取消/被隐藏或拖走不动作）；自由摆放则常驻；
  ⑤双分隔线分三组：岛操作（显示/隐藏）｜开窗（报表/设置/关于）｜退出。
  refresh_toggle_text 随之简化为纯 is_visible（不再读 motion），peek_apply/apply_snap
  内的刷新调用移除（可见性不因贴边滑入滑出改变）
- 待所有者 dev 验收：贴边隐藏态托盘"显示灵动岛"滑回、菜单文案随状态切换、
  两处额度 tooltip 应正常换行
- 依赖：无

### M1-9 托盘菜单自绘改造 ✅（2026-09-20 补记；代码完成并提交 8cb5248）

- 内容：webview 迷你状态面板替代原生菜单（muda 无样式 API）——头部状态/消耗/额度
  三行信息架构、托盘左键行为可配置、窗口高度内容自适应、面板提亮、召唤首帧时序修复；
  动作经 tray_menu_action 复用旧托盘逻辑，数据零新增查询（复用 island-snapshot 广播）
- 补记说明：开发与提交（8cb5248）当时未回写本看板，此条为 M1-10 立项时按提交信息补记
- 依赖：无

### M1-10 会话中心 ✅（2026-09-20 完成，所有者 dev 验收通过）

- 触发：所有者提出「面板会话过多不便查看＋缺会话细节入口」；经两轮方案讨论拍板：
  报表页移除会话明细，新增独立会话窗口专门管理查看所有会话（分析与管理分离，
  状态筛选等会话语义不再与报表聚合筛选链耦合）
- ✨ 会话窗口（#sessions 路由，第五个辅助窗口，照设置/报表同模式）：
  - 分页表格每页 20（可选 20/50/100，store 侧钳制 ≤200）：序号（筛选结果内跨页
    连续自增）/状态/会话/Agent/模型/项目/最近/次数/出错/时长/Token，
    Token 悬浮四项拆解、出错列悬浮错误类型列表（GROUP_CONCAT DISTINCT）
  - 状态档 chips（全部/进行中/已结束/有错误）：口径与岛面板同源
    （idle 超 2h=已结束，ENDED_AFTER_MS 前端 shared/sessionDisplay 与 Rust store 双侧常量同步）；
  - 关键字搜索（标题/项目路径，LIKE 转义＋300ms 防抖）；排序四键
    （最近活动/Token/次数/时长，白名单防注入，方向固定降序）
  - 范围五档默认「今日」（所有者拍板，初版曾定近 7 天后调整）；Agent/项目/模型筛选复用
    SearchSelect（自报表页抽出至 shared，附独立样式文件，避免 echarts 漏进会话 chunk）
  - 行点击跳转仅限 30 分钟内活跃会话（历史会话进程必退，跳转必未命中）；
  - CSV 导出（所见即所得：当前筛选＋状态＋关键字＋排序的全量，BOM＋本机时区）；
    数据截至时间戳＋手动刷新（不做自动轮询）；错误行内提示＋重试（不弹窗，红线⑤）
- ✨ 岛面板收口：活跃区上限 12/历史展开区上限 20（所有者确认），溢出显示
  「查看更多会话」（hover：点击查看更多会话数据）直达会话窗口；
  标题行「会话 · N / M」变为可点击入口
- 🔧 报表页瘦身：会话明细区块/翻页器/导出按钮移除，回归纯聚合分析（页脚指引会话窗口）
- 🔧 Rust：store session_page（内层聚合子查询＋外层状态/关键字过滤的参数化查询，
  排序/状态档白名单校验）＋build_sessions_csv＋session_options（轻查询，下拉选项
  免拉整页报表快照）；lib.rs 会话命令全部走 run_report 阻塞线程池（审查 2.2.1 模式）；
  report_sessions/export_report_csv 命令随迁移删除
- 🐛 单测逮住回归一处：CSV 子查询曾把时间格式化成文本后再做「空闲超 2h」数值比较，
  SQLite 类型序 TEXT>INTEGER 致 ended 档全空——改为内层保毫秒整数、外层才格式化
- 验证：cargo test **30/30** 通过（8 个集成测试按设计 ignore）、npm run build 通过
  （Sessions chunk 8.5KB 独立加载，echarts 仍仅报表 chunk，岛主包不受影响）
- ✅ 验收：所有者 dev 验收通过（2026-09-20），含验收期打磨——序号列/每页行数/默认今日、
  列级口径迁移表头悬浮（Tip 新增 top 弹位＋长文案换行）、表头与数据列错位修复、
  下拉样式缺失修复、错误红字/已结束置灰/徽标同色三处口径对齐、「查看更多会话」文案精简
- ⚠️ 并行开发注意：本任务开发期间 lib.rs 岛显隐逻辑由所有者并行重构
  （peek_apply → island_transition 状态机），会话相关改动已在新结构上验证通过
- 依赖：M1-9（托盘菜单形态，已提交 8cb5248）

### M1-11 会话数据深利用：指标补全＋会话详情抽屉 ✅（2026-09-20 完成，所有者 dev 验收通过）

- 触发：所有者问「库里还有哪些会话数据可充分利用」，盘点后拍板两档都做
- ✨ 档一（列表指标补全，改聚合查询＋悬浮提示）：
  - `session_page` 补思考 token（`SUM(reasoning_tokens)`）与平均首字（`AVG(ttft_ms)`）两列，
    CSV 同步加「思考/平均首字(毫秒)」列；
  - Token 悬浮加思考及占比（口径同报表：思考/(输入＋输出＋思考)，不含缓存）、
    平均单次 token、首末相隔（墙钟跨度）；次数悬浮平均 token/次；
    出错悬浮错误率；时长悬浮平均首字（仅有 ttft 记录的会话）
- ✨ 档二（会话详情抽屉）：点击任意行 → 右侧抽屉展示——
  - 「调用流水」：`usage_records` 按会话倒序（上限 500），单卡＝时间/模型/入/出/思/缓/
    时长/首字/错误标签，出错卡红框；快速切换行丢弃过期响应；
  - 「状态时间线」：`status_events` 双口径查询（hook 原始 id＋"{agent}:{id}" 命名空间 id
    都能命中，单测覆盖），上限 200，悬浮看原始 payload；
  - 跳转入口从行点击迁入抽屉（仅近 30 分钟活跃可用）；行点击一律开抽屉；
  - 遮罩点击/Esc 关闭；右侧滑入动效复用全局缓动令牌
- 🔧 顺带：`fmtMs` 抽至 shared/format（报表页同款本地实现收编）；
  🐛 **Agent 自定义颜色窗口差异修复**：岛窗口有 agent_colors 注入＋agents-changed 订阅，
  会话/报表窗口没有——自定义颜色不生效（用户实测踩中）。新增共享
  `shared/useAgentColors` Hook（读设置注入＋实时跟随；带 1s 重试防早期 invoke 丢失，
  岛窗口同款教训），会话与报表窗口均已接入
- 🐛 单测逮住两处：①CSV 首字列 `COALESCE(整数,'')` 类型混用致 rusqlite 按 String 取值
  整行丢弃（补 CAST AS TEXT）；②状态事件双口径测试样本写错命名空间（修正断言）
- 验证：cargo test 30/30、npm build 通过、浏览器桩数据实渲染（抽屉/悬浮/换行均正常）
- ✅ 验收：所有者 dev 验收通过（2026-09-20）；状态时间线的真实事件丰富度随 hooks 启用逐步体现
- 依赖：M1-10

### M1-12 数据准确性治理：双计根治＋后台用量补采 ✅（2026-09-21 代码完成，待所有者 dev 验收）

- 触发：所有者提出「数据更新不准确」，全链路核查（采集→存储→查询→展示）＋本机真实数据
  四方对账实证，定位出两个实锤误差源与三个口径/健壮性问题（对账方法与数字见 01-RESEARCH §8 补记）
- 🐛 **双计根治（活跃日实测虚高 +27.8%，今日 +19.4 万 token）**：CC 同一 assistant 消息平均
  写 2~3 行纯复制流式快照（usage 全同、timestamp 各异，1~10 秒差居多，实测 492/534 条消息），
  旧幂等键 `(agent, session_id, ts, model)` 不含消息身份——工具运行期间这些行跨采集轮次
  分裂入库各自成行；工具未运行期间写入的数据靠启动后一轮全量回溯＋调用内 message.id 去重
  恰好无误，故历史天误差 0.0%、唯独活跃日虚高。修复：迁移 0003 重建 usage_records，
  幂等键升级为 `(agent, session_id, source_id)`（CC=message.id(+requestId)，ZCode=源库行 id），
  同源多行冲突时保留最大快照（原口径不变）
- ✨ **后台用量补采（缺口 133 万 token，占 4.55%）**：CC 转录 cost-state 行携带会话级累计
  `modelUsage`（camelCase、模型名带 `[1m]` 后缀、无 ISO timestamp 只有毫秒 startTime），
  按（会话×归一化模型）取最大累计快照，超出 assistant 明细的部分＝标题生成等后台调用的
  真实消耗，以差值行入库（`source_id='cost:…'`、`is_background=1`、无条件覆盖跟随重算）；
  token 计入消耗口径，调用次数不计（today_usage/报表趋势/维度分组/热力图/会话页 calls
  统一切换为 `SUM(NOT is_background)`），调用流水与最近模型展示排除后台行
- ✨ **存量清洗＝自动清空重建**（所有者拍板）：0003 迁移重建空表＋写 `usage_rebuild_pending`
  标志，聚合器构造时检测后归零水位，首轮采集全量回溯自动收敛（数据源 CC 转录/ZCode 源库
  都是完整事实源；现存转录最早 08-30 与自库最早一致，重建零损失）
- ✨ **水位时钟防线**：`set_watermark` 前钳制 `min(max_ts, now)`——行时间戳若因时钟回拨
  出现未来值，旧实现水位被永久推高后所有新行被过滤（数据静默停更）
- 🐛 **取消不算错误**（所有者拍板）：ZCode 采集 SQL `CASE WHEN cancelled_by_user=1 THEN NULL`，
  用户主动 ESC 不再计入出错次数/错误分布（存量 21 行随重建自然清洗）
- 🐛 **冻结状态修复**：会话中心/CSV 的 active 判定统一为「state != 'offline' 且最近活动在
  ENDED_AFTER_MS 内」——原三态（working/waiting/error）无时间条件，会话离开 90 天观测
  列表后 sessions.state 冻结会被永久当"活跃"
- 🐛 **0003 落地修复＝迁移 0004**（所有者实测"标题没解决"后深挖发现）：0003 首版 SQL 的
  `CREATE TABLE IF NOT EXISTS` 在已有旧表的库上被静默跳过、但 user_version 已消耗到 3——
  此类库表停留在旧 13 列结构（无 source_id/is_background），新代码写入全部失败/旧逻辑持续
  双计，且修复版 0003 永远不会再执行。0004 无条件重建为同款新结构，任何中间状态一次收敛
  （已是新结构的库多一次清空重建，全量回溯幂等恢复）；回归测试模拟"ver=3＋旧 13 列"被骗库
  验证自动修复
- 🔧 **first_cwd 读取量 8KB→64KB**（所有者截图显示编码目录名回退）：头部可能连续多行
  summary/attachment/ai-title 等不带 cwd 的行，实测 8KB 仅覆盖 19/43 文件、64KB 覆盖
  43/43；cwd 缓存按 mtime，重启后自动以新逻辑重读
- 🔧 顺带发现并修复：ZCode 源库 `model_usage.id` 实测 TEXT 型（按 i64 读全部行解析失败）；
  cost-state 行无 `timestamp` 字段回退 `startTime`
- ✨ **CC 会话标题补采**（所有者实测反馈"标题显示的是目录名"）：CC 终端的会话标题就写在
  转录 `ai-title` 行（`{"aiTitle":…,"sessionId":…}`，随对话推进多次重写、散布全文，
  实测 185 行/41 文件）——此前 scan 未解析、title 恒 None，前端 cardTitle 回退链落到
  项目目录名。修复：`collect_usage` 增量解析顺路带出（后写覆盖=最新标题，零额外 IO），
  `CollectOutput.titles` 透传 → upsert_session 与 SessionView 兜底（COALESCE 保旧不丢）；
  0003 重建首轮全量回溯恰好把全部存量标题一次补齐；无 timestamp 字段故不做水位过滤。
  ZCode 不受影响（session 表自带标题）。
  🐛 快照标题丢失二次修复（所有者截图实测踩中）：岛面板 SessionView 每轮快照重建，
  兜底只挂"本轮恰好采到新标题"——文件无新行时标题消失回退目录名。加第三级来源
  `store.session_titles` 批量读 sessions 表持久值（与 latest_session_models 同款模式），
  标题链=scan 自带（ZCode）> 本轮新采集 > 已入库持久值
- 🐛 **会话窗口 CC 模型列为空修复**（所有者截图实测踩中）：sessions.model 对 CC 恒 NULL
  （scan 无模型信息、COALESCE 保 NULL），会话窗口直读表显示 —，岛面板走 latest_models
  回填所以正确——两处口径不一致。修复：聚合器 upsert 时把回填后的模型持久化进
  sessions.model（最近一次调用模型，与岛面板同口径），所有读表展示位统一
- 结构改动：`AgentAdapter::collect_usage` 返回 `CollectOutput { rows, cost_snapshots, titles }`
  （ZCode cost_snapshots/titles 恒空）；service 层用 `assistant_model_total` 点查重算差值，
  `last_cost_cum` 内存缓存避免重复重算
- 验证：cargo test 31/31（新增回归：同 source_id 异时戳合并/后台行无条件覆盖/calls 排除）、
  ignored 集成 8/8（真实 CC/ZCode/hooks e2e/聚合器）、cargo check 零告警、npm build 通过；
  **端到端对账**：临时库跑真实聚合器重建，今日 CC=696,302 与现存文件真实口径分毫不差
  （修复前自库 889,947），总量 30.48M ≈ assistant 29.1M＋后台 1.33M
- ⚠️ 验收注意：首次启动会执行 0003 迁移并清空用量表，随后 10 秒内全量回溯恢复
  （岛面板 token 短暂从 0 涨回属预期）；「今日消耗」数字会比修复前变小（去掉双计）
- 依赖：无（独立修复批次）

### 已划出 M1（2026-09-17 所有者拍板）

- Anthropic 官方订阅额度 → 移回 M2 愿景池（所有者无官方订阅）
- NSIS 安装包 → 归正式发布阶段（WORKFLOW 构建打包纪律）
- 悬浮球形态 → 不做（M1-6 贴边自动隐藏已覆盖其价值，详见 M1-5 条目）

## M2 任务序列（2026-09-22 启动，总纲见 [04-EXPANSION](04-EXPANSION.md)；M1-3 Codex 接入由 M2-6 吸收升级）

### M2-1 调度器骨架：唤醒环＋自适应档位＋快轮＋广播去重 ✅（2026-09-23 所有者 dev 验收通过）

- 内容：`spawn_aggregator` 固定 10s sleep 改 `recv_timeout` 唤醒环；自适应档位
  （工作中/等待 1s｜有会话全空闲 5s｜无会话 10s）；快轮线程 1s 采样各适配器
  `HotSignal`（hook 事件文件/当日日志/转录树浅枚举），值变化投递唤醒
  （channel 容量 1 合并风暴，MIN_TICK_GAP_MS=250 护栏）；广播签名去重
  （内容哈希含 sessions/island/quotas/today_*，generated_at 除外，不变不 emit）
- 涉及：src-tauri/src/lib.rs（spawn_aggregator 重写＋now_ms 助手）、collector/mod.rs（trait 增 hot_signals/process_match 默认方法）、collector/engine.rs（新建：HotSignal/SignalValue 采样器）
- 验收（04-EXPANSION M2-1）：✅ CC/ZCode 发消息后 ≤2s 岛变呼吸绿（日志时间戳验证）；✅ 空闲 10 分钟 CPU<1%；✅ 签名去重生效无每秒快照风暴（2026-09-23 所有者验收）

### M2-2 ZCode 活动信号升级 ✅（2026-09-23 所有者 dev 验收通过）

- 内容：`rollout/model-io-sess_*.jsonl` per-session mtime 纳入 scan_sessions 的
  last_usage_at（取与库内 MAX(started_at) 较大者）；当日日志 `zcode-日期.jsonl`
  作为快轮信号（路径按日现算，跨零点自动切换）
- 涉及：collector/zcode.rs（rollout_mtimes + scan 增强 + hot_signals）
- 验收：✅ 长回答（>90s）生成期间保持 working 不掉 idle（2026-09-23 所有者验收）

### M2-3 引擎抽取与迁移 ✅（2026-09-23 所有者 dev 验收通过）

- 内容：新建 collector/engine.rs（GlobWalker 递归枚举＋目录 mtime 剪枝缓存、
  IncrementalFileReader 64KB 回退增量读、ScanBudget 节流、open_sqlite_readonly、
  ProcessMatch）；CC 适配器瘦身（枚举/增量读迁出，解析/cwd 缓存保留）；
  ZCode 适配器接入 ScanBudget（scan/collect 各 2s 预算，缓存返回）＋只读打开迁移；
  hook 消费偏移键 per-agent 化（`hook_events_offset` → `hook_events_offset:claude-code`，
  首启自动迁移）。对外 CollectOutput/SessionInfo/幂等键结构不变
- 涉及：collector/engine.rs（新建）、collector/mod.rs、collector/claude_code.rs、
  collector/zcode.rs、state/service.rs（偏移键迁移）、store（无 schema 变更）
- 验收：✅ 升级前后真实数据逐分项一致（2026-09-22 当日对账：aggregator 87 会话/CC 分项
  正常；7 天水位/偏移稳定性随 M2-5 观察期一并确认）；✅ 重启后水位/偏移行为不变

### M2-4 进程匹配声明化 ✅（2026-09-22 代码完成）

- 内容：`probe_processes` 的 zcode/claude 硬编码匹配迁入各适配器
  `process_match()` 声明（ProcessMatch{name/cmd_keywords + cmd_excludes}），
  service 层零 per-agent 分支，probe_cache 改 {agent_id: 存活} 表——
  14 家接入的硬前提（04-EXPANSION §2.6），新 Agent 声明即接入
- 涉及：collector/mod.rs、claude_code.rs、zcode.rs、state/service.rs

### M2-5 P0.5 评估（notify 事件驱动） ⬜

- 判据（量化，04-EXPANSION M2-5）：快轮档达成 ≤2s 首信号延迟的前提下，
  仅当「空闲期 CPU>2%」或「用户主观仍觉迟滞」才启动 notify；否则降 backlog

### M2-UX-1 面板历史区可见性治理：纯时间序＋Agent 筛选器 ✅（2026-09-23 所有者 dev 验收通过）

- 背景（所有者报障「面板漏了 CC 会话」）：后端扫描正常（tick 摘要 42 ZC＋45 CC=87），
  根因是展示层——①sortSessions 把底层状态权重（idle=3/offline=4）带进历史区，
  ZCode 进程常开→其 idle 型历史会话（41 个）整体压制 CC 的 offline 型（45 个），
  与最近活动无关地钉死队尾；②历史区截断 20 条，CC 全部在截断线后 → 用户观感"漏了 CC"。
  会话窗口可见是因为走 SQL「今日」查询（仅 3 条），两处数据源本身都完整
- 改动：
  ① `sessionDisplay.ts` 新增 `sortHistory`（历史区分流后纯最近活动降序，
     同毫秒按 token 兜底防跳位）；`sortSessions` 不动（活跃区语义保留，窗口不受影响）
  ② `Panel.tsx` 历史区标题行内嵌 Agent 筛选器（「全部 ▾」触发 → 内联展开菜单：
     按会话数降序=常用自浮、徽标色点+实时计数、max-height 内部滚动 14+ 家可扩展；
     筛选中标题行回显「仅 CC ✕」、Tooltip 对账口径同步「命中 N/共 M」；
     点击外部/Esc 关闭；面板收起随卸载自动复位）；「查看更多会话」带 Agent
     预筛选跳转（emit sessions-prefilter）
  ③ `Sessions.tsx` 监听预筛选事件自动选中对应 Agent（闭环）
  ④ `engine.rs` GlobWalker 加固（随行修复）：read_dir 瞬态失败不缓存空结果
     （旧实现每轮直读失败只影响当轮；带缓存的空结果会造成目录 mtime 不变
     期间的永久失明）——纯 Rust 侧健壮性回归
- 设计评审结论（2026-09-23 所有者拍板）：筛选器形态=标题行内嵌+内联菜单
  （否决横向 chips：380px 宽度 14 家必溢出；否决纯徽标点击：鸡生蛋——
  沉底的卡看不见就没得点）；排序差异本身保留（面板状态优先 vs 窗口列排序
  是 M1-10 有意设计，服务不同场景）
- 涉及：src/shared/sessionDisplay.ts、src/island/Panel.tsx、src/sessions/Sessions.tsx、
  src/App.css、src-tauri/src/collector/engine.rs
- **交互升级（2026-09-23 所有者拍板）**：筛选菜单从点击展开改为**纯 hover 交互**——
  悬浮触发器即展开、点选即应用并关闭、移出触发器+菜单整体区域 200ms 宽限自关
  （Esc 兜底）；折叠头保持点击展开列表不动；标题行改整体 hover 热区（行级背景），
  触发器三态（默认低调/行悬浮增强/自身悬浮强调色），筛选激活常显强调色；
  菜单入场淡入+微移动画（推卡片变有意动画）；触发器 .open/.active 态 +
  aria-expanded（键盘不可达为已声明取舍）。实现参数：FILTER_OPEN_DELAY_MS=0
  （误触多可调 100）、FILTER_CLOSE_DELAY_MS=200
- **交互细化二（2026-09-23 所有者拍板）**：筛选触发器**仅历史区展开态可见**
  （渐进披露——收起态筛选是「看不见列表的筛选」，选完还得再展开=死路）；
  配套规则：**收起历史区自动清除筛选并关闭菜单**（防「看得到计数却改不了
  筛选」的反向死角），与「面板收起自动复位」哲学一致
- **交互细化三（2026-09-23 所有者反馈）**：①菜单固定窄宽 190px 靠右对齐触发器
  正下方；②保活域从「整行+菜单」缩小为「触发器+菜单」——指针移到折叠头区域
  即由 200ms 宽限自动收起菜单，不再常开
- **交互细化四（2026-09-23 所有者反馈）**：筛选菜单从内联块改为 **portal 浮层**
  （挂 body + fixed 定位 + 两段式测量翻转，照抄 Tip 范式）——展开不再推挤下方
  历史卡片（内联块占位是内联方案的固有代价，实测体验差）；玻璃配方实面板一档
  ＋投影保证盖住卡片可读；会话区滚动即自动关闭（capture 监听，菜单自身滚动除外）；
  触发器/菜单 200ms 宽限桥接、点选即关等既有交互全部不变
- 验收：✅ 面板历史区展开后 CC/ZC 混排可见（不再 ZC 钉尾）；✅ 筛选仅 CC
  命中 45；✅ 「查看更多」跳转后会话窗口已预选 CC；✅ 活跃区不受筛选影响；
  ✅ hover 展开/宽限关闭/选完即关手感符合交互规格（2026-09-23 所有者验收）

### M2-6 Codex 适配器 ✅（2026-09-23 代码完成，真实对账待装机）

- 内容：`collector/codex.rs` 新建——`$CODEX_HOME/sessions/**/rollout-*.jsonl` tail
  （GlobWalker 日期分区+revert 变体归并 thread id）；用量取 `token_usage_record`
  行（response_id 幂等键），`event_msg→token_count` 作旧文件退路（last_token_usage），
  **同轮增量两通路互斥防双计**；cached_input_tokens 按 OpenAI 子集语义拆分
  （input -= cached，对齐四项互斥口径）；model 随 turn_context 行逐 turn 更新
  （per-file 缓存）；CODEX_HOME 重定向（校验目录存在）；hooks 注入器走 config.toml
  `[hooks]` 段（toml_edit 保格式+备份+原子写，8 事件，形状按 hook_config.rs serde
  源码核对）；桥脚本 argv 参数化（一份脚本服务多家）
- 调研先行（所有者拍板不装机）：openai/codex 源码级核实（serde 属性级），
  详 01-RESEARCH §11.1
- 验收：✅ 合成样本单测（解析/互斥/幂等/水位/日期分区/thread id 提取/hooks 装卸往返）；
  ✅ cargo test 全绿；⬜ `test_real_codex` 对账留待装机（已写好标 #[ignore]）

### M2-7 Kimi Code 适配器 ✅（2026-09-23 代码完成，真实对账待装机）

- 内容：`collector/kimi.rs` 新建——**纠正总纲旧版记录后按新版 kimi-code 实施**
  （TS v2.0.2；`~/.kimi-code`，KIMI_CODE_HOME 重定向）：`sessions/**/wire.jsonl`
  tail、`usage.record` 行解析（camelCase 四字段，无 reasoning 独立字段）、
  行无官方 id → 内容指纹幂等（kr:{agentId}:{time}:{四项}）、state.json 取
  title/cwd/updatedAt（mtime 缓存）、hooks TOML 注入器（顶层 `[[hooks]]`，
  12 事件含 StopFailure/PostToolUseFailure）；
  **状态机扩展**：SessionSignals 新增 last_failure 信号位（失败事件窗口内直接
  判 error，04-EXPANSION §2.7 唯一数据结构扩展）；事件映射表补差异集
  （PermissionRequest→Waiting、Interrupt→Idle、TurnStarted/TaskStarted→Working、
  失败类→Error，未知事件退启发式不变）
- 调研先行：MoonshotAI/kimi-code 源码+文档核实，详 01-RESEARCH §11.2
  （含旧版/新版纠正记录）
- 验收：✅ 合成样本单测（wire 解析/指纹稳定/水位/state.json 元数据/hooks 装卸往返/
  last_failure→Error 映射）；✅ cargo test 全绿；⬜ `test_real_kimi` 对账留待装机
  （usageScope 双计验证点已登记 §11.3）

### M2-8 前端登记 ✅（2026-09-23 代码完成；行级整合为所有者拍板二轮改版）

- 内容：`AGENT_DEFS` 新增 codex（#38bdf8，原预占色转正）/kimi-code（#f472b6），
  两家 implemented: true（岛/面板/会话中心/报表/筛选器全链路数据驱动自动适应）；
  lib.rs hooks_status/install/uninstall 命令带 agent 参数（不支持的 agent 报错）
- **二轮改版（所有者拍板「行级整合」，方案 A）**：hooks 三行卡片从「数据与维护」
  迁出——每个 Agent 勾选行内嵌「精确」小号开关（仅 HOOKS_AGENTS 三家有），
  行结构 = 勾选+名称+精确开关+色块（check 列 flex:1 使开关+色块右对齐成组，
  各行垂直对齐）；开关状态常显（注入与否一眼可见），busy/未勾选时禁用置灰
  （不监控谈不上精确），完整说明收进 hover title；Switch 组件增 small/disabled
  变体；「数据与维护」恢复纯运维两行（保留时长+开发者模式）
  - 否决方案 B（章节内分区：视觉两张皮、14 家时章节冗长）与
    方案 C（渐进披露展开次行：hooks 状态不可见+层级深，过度设计）
- 涉及：src/shared/types.ts、src/Settings.tsx、src/settings.css、src-tauri/src/lib.rs
- 验收：✅ npm run build 通过；✅ 浏览器渲染自查（vite #settings 直开：
  800px/520px 双宽度下三行开关右对齐不挤压，DOM 结构/数据与维护区均符合设计）；
  ⬜ 设置页行内开关装卸实机操作留待所有者 dev 验收

### M2-9 索引复查 ✅（2026-09-23 完成，结论：无需迁移）

- 内容：EXPLAIN QUERY PLAN 复查（真实库 4607 行）：会话中心聚合内层
  `SCAN u USING INDEX idx_usage_session`（GROUP BY 走覆盖索引，非表扫描）；
  today_usage/趋势/分布走 idx_usage_ts 范围查找；详情抽屉流水走 idx_usage_session——
  重查询均无全表扫描，现有索引已覆盖，**不产出 0005 迁移**；
  会话量×14 后同构查询计划不变（索引与 agent 维度无关）
- 验收：✅ EXPLAIN 五类重查询全部命中索引（记录于本条目，无 schema 变更）

### M2-P1 hooks 链路多 Agent 泛化 ✅（2026-09-23 代码完成，M2-6/7 硬前提）

- 内容（04-EXPANSION §2.3.3 落地）：hook_events.rs 事件文件 per-agent 化
  （`events/<agent>.jsonl`，claude-code 文件名不变零迁移）；桥脚本 argv 接
  agent 标识（缺省 claude-code 向后兼容旧注入；Kimi errorMessage 归入 message
  透传）；hook 消费循环按适配器逐家执行（偏移键 `hook_events_offset:<agent>`
  M2-3 已就绪，逐家轮转）；`sig.last_hook` 去掉 claude-code 硬编码改通用喂给；
  新增依赖 toml_edit（Kimi/Codex config.toml 保格式注入）
- 涉及：src-tauri/hook-bridge/hook-bridge.js、collector/{hook_events,claude_code,
  codex,kimi,mod}.rs、state/{mod,service}.rs、Cargo.toml
- 验收：✅ CC hooks 装卸往返回归通过；✅ 桥脚本缺省参数向后兼容；
  ✅ cargo test 49/49 全绿；⬜ Codex/Kimi hooks 实机触发链留待装机

### M2-10a OpenCode + MiMo Code 适配器（同族 SQLite 通道）✅（2026-09-23 所有者验收通过）

- 背景（源码调研推翻总纲预想，详 01-RESEARCH §12）：OpenCode 主存储**已迁 SQLite**
  （v1.18.32 实测 `~\.local\share\opencode\opencode.db`，JSON storage 仅剩边缘用途）；
  MiMo Code 为 OpenCode fork 确证（`~\.local\share\mimocode\mimocode.db`，
  `MIMOCODE_HOME` 重定向）。通道优先级反转：**只读 SQLite 为主通道（零用户配置），
  SSE 降为 opt-in 增强档**（M2-10b 装机后实施——端口发现/事件流实测为前提）
- 内容：`collector/opencode.rs` 新建——`OpenCodeFamilyAdapter` **同族参数化**（两家
  共用一 struct，差异仅 agent id/数据根解析/db 文件名，SQL 与 data JSON 解析
  零复制粘贴）：session 表 scan（title/directory/agent/model）+ message 表水位
  增量（`json_extract(data,'$.role')='assistant'` 行提取 tokens 五项+cost+model，
  `oc:`/`mc:msg_{id}` 幂等键）+ `$.error` 行喂 error_type 走现有 recent_error 链路
  + `-wal` 文件快轮信号（WAL 写入先落 wal，主 db mtime 不动）+ 进程匹配声明；
  自库零迁移、无 hooks（两家无 CC 式 hooks 体系）
- 涉及：src-tauri/src/collector/{opencode,mod}.rs、state/service.rs、
  src/shared/types.ts、docs/{01-RESEARCH,04-EXPANSION,03-TASKS}.md
- 验收：⬜ 合成样本单测（临时库灌样本：用量提取/水位增量/幂等/重定向/错误行）；
  ⬜ cargo test 全绿 + npm build；⬜ `test_real_opencode`/`test_real_mimo`
  对账留待装机（清单 01-RESEARCH §12.3）

### M2-UX-2 设置页 Agent 监控卡片网格改版 ✅（2026-09-23 所有者验收通过，含色条两轮迭代与间距对齐）

- 背景：所有者两项指示——删掉 Claude Desktop 选项；六家展示方式升级（方案 A 卡片
  网格获拍板，列数改为纯宽度自适应＋4 列封顶）
- 内容：①AGENT_DEFS 删 claude-desktop 行＋implemented 字段（引用仅 Settings 一处
  死分支，一并清）；②Agent 监控章节渲染重写为**双列卡片网格**：`auto-fill +
  minmax(min(280px,100%),1fr)` 容器 1280px 封顶——实测分档 1000px→3 列/
  760px→2 列/520px→1 列，极端窄自动退单列不截断；③checkbox→Switch（语义归位）；
  **身份色收敛为「色点＋开关」对角呼应**（设计两轮迭代：全高色带→短色条均因与
  卡片圆角/色点扎堆不协调被所有者否掉，终版按「去掉一件饰品」减法原则删色条，
  启用开关点亮时经 `.st-agent-on` 局部覆写 `--accent` 随身份色，每卡仅两个
  同色元素且皆有语义）＋**自绘色点**取色入口（原生 color input 隐藏为点击代理，
  hover 微放大提示可点）；④副标题=真实采集方式三态文案
  （已注入/未注入约 90 秒精度/进程与文件启发式/未启用）；⑤整卡可点切换启用
  （开关/色点/精确开关 stopPropagation 隔离）＋键盘可达（Enter/空格＋focus-visible）
- 涉及：src/shared/types.ts、src/Settings.tsx、src/settings.css
- 验证：✅ npm run build；✅ 浏览器渲染自查（vite #settings 直开）：深浅双主题
  （浅色经 data-theme 强制切换验证）×1/2/3/4 列四档宽度截图逐一核对，列数分档
  与 textClipped=false 全部断言通过；交互 DOM 级验证（点卡切换 aria-pressed
  联动、副标题变「未启用」、精确开关不误触卡片）；⬜ 真机主题切换联动待所有者
  dev 验收（直开环境主题保存链路不可达属预期）

### M2-11 Gemini CLI + Qwen Code 适配器（fork 已分叉的独立双适配器＋hooks 注入）✅（2026-09-23 所有者验收通过）

- 背景（两轮源码调研推翻总纲三处预想，详 01-RESEARCH §13）：①「Qwen 同族参数化
  换根即用」不成立——Qwen Code fork 基线 gemini-cli v0.8.2 且自 v0.1 起停止同步，
  落盘结构已大幅分叉（Qwen=projects/<sanitizeCwd>/chats/<uuid>.jsonl 纯追加消息树，
  Gemini=tmp/<slug>/chats/session-*.jsonl 含 $set/$rewindTo 控制行＋同 id 重 append），
  故两家**各自独立适配器**（强行 Family 参数化违反 YAGNI），仅共享 token 拆分口径；
  ②Gemini「无 hooks」预想不成立——现有 11 事件 CC 式 hooks；Qwen 22 事件且几乎
  全 CC 同名（PermissionRequest/StopFailure/PostToolUseFailure 直通状态机）；
  ③token 口径裁定：两家均 cached ⊆ prompt（OpenAI 语义），官方 /stats 明确
  input = prompt − cached，入库前拆分
- 内容（所有者拍板扩大范围：主通道＋hooks 注入一并做）：
  ①`collector/gemini.rs`：`tmp/*/chats/session-*.jsonl` tail（GlobWalker 多段
  pattern，子代理嵌套两层天然排除）＋首行 metadata 头缓存（sessionId/directories/
  首条 user 标题兜底，内容永不变缓存永续）＋`gm:{id}` 幂等（同 id 重 append 靠
  库层 UPSERT 后写覆盖，同轮保留用量大者）＋`$set` 行只取 summary 作 titles
  （忽略 messages 全量数组）＋type:error 行喂 recent_error＋`GEMINI_CLI_HOME` 重定向；
  ②`collector/qwen.rs`：`projects/*/chats/*.jsonl` 纯追加 tail（`*.jsonl` 天然排除
  runtime.json sidecar 与 legacy tmp 目录）＋文件名即会话 uuid＋行内 cwd 直取
  （sanitizeCwd 不可逆无需反解）＋`qw:{uuid}` 幂等＋custom_title 多指针宽容提取
  ＋`QWEN_HOME`/`QWEN_RUNTIME_DIR` 双重定向；③hooks：两家 settings.json JSON 注入，
  **注入机制第三次重复即抽象**——CC 注入核心迁入 engine.rs 公共化
  （inject_json_hooks/uninstall_json_hooks＋原子写/备份清理），三家共用；
  Gemini 注入 7 事件（BeforeAgent/AfterAgent/BeforeTool/AfterTool 语义就近映射，
  ⚠️ 无 async 字段同步执行、timeout 单位毫秒 5000）；Qwen 注入 10 事件
  （timeout 秒＋async:true 零阻塞＋shell 显式 powershell 避开三态不确定性）；
  状态机映射表＋4 个 Gemini 差异事件（state/mod.rs）；HOOKS_AGENTS 3→5 家
- 涉及：src-tauri/src/collector/{gemini,qwen,engine,claude_code,mod}.rs、
  state/{mod,service}.rs、lib.rs、src/shared/types.ts、
  docs/{01-RESEARCH,04-EXPANSION,03-TASKS}.md
- 验收：✅ cargo test 61/61 全绿（新增 6：两家解析/增量/hooks 往返装卸）；
  ✅ cargo check 零告警；✅ npm run build；✅ 所有者 dev 验收通过（2026-09-23）；
  ⬜ `test_real_gemini`/`test_real_qwen` 对账留待装机（清单 01-RESEARCH §13.3）

### M2-12 OtelSink 骨架（Gemini/Qwen 的 OTel outfile 增强通道）✅（2026-09-23 所有者验收通过）

- 背景（outfile 字段级源码调研新确证五点，详 01-RESEARCH §13.4）：①Gemini 每次
  API 响应写**两条** log（api_response 全量 6 项计数＋semantic 摘要仅 2 项）——不按
  event.name 白名单过滤必双计；②Qwen 为**单条**记录且形态分叉（token 计数在
  attributes 顶层、无 tool 项、多 response_id/ttft_ms）；③outfile 每条 log 记录的
  JSON 只有 resource/instrumentationScope/attributes 三个可枚举键（OTel sdk-logs
  0.218.0 时间戳/body 私有不落盘），时间取 attributes.event.timestamp；
  ④outfile 启用硬前提 telemetry.enabled=true＋outfile 有值（缺一不落盘），发现链
  argv ?? env ?? settings.telemetry.outfile；⑤api_error 两家都有且比转录 error 文本
  精确（带 error_type/status_code）
- 内容：①`collector/otel.rs` 新建——OtelProfile 两家差异静态声明（提取逻辑零
  per-agent 分支）＋outfile 发现（settings JSONC 容忍＋env 覆盖＋mtime 缓存＋`~/`
  展开）＋pretty JSON 值流游标（serde_json StreamDeserializer 按值配平＋精确字节
  偏移推进＋截断半条停值起点待补读＋UTF-8 尾部多字节残缺只解析合法前缀＋中段坏
  数据防卡死跳行）＋白名单提取（api_response→token 行/api_error→error 行，span/
  metrics/semantic 全跳过）；②两家适配器组合挂载（无新引擎管线——总纲 §2.3.4
  「独立 OtelSink 引擎」在 outfile 模式下本质是单文件 tail，独立成引擎属过度设计，
  实施裁剪回写总纲）：hot_signals 增 outfile 信号、collect_usage 尾部并入；③隐私
  红线（总纲 §2.8.2）：logPrompts=true 时 attributes 携带 prompt/response 全文，
  解析白名单取数只碰数字/模型/会话 id/时间戳/错误类型
- **通道裁定（所有者拍板 2026-09-23）**：api_response token 行**解析与对账就绪但
  暂不入库**——与转录通道（M2-11 已采）是同回合两份记录且无公共 id 可对齐，入库必
  双计；api_error 行直接入库走 recent_error（转录/hooks 均无的精确信号，token 全
  None 无冲突）。装机对账后若切 outfile 主通道，适配器侧把 batch.rows 一并并入即可
- 涉及：src-tauri/src/collector/{otel,gemini,qwen,mod}.rs（service/lib/前端零改动）、
  docs/{01-RESEARCH,04-EXPANSION,03-TASKS,HANDOFF}.md
- 验收：✅ cargo test 71/71 全绿（新增 10：otel 8——流解析/白名单防双计/截断续读/
  UTF-8 残缺/坏段防卡死/重建归零/错误行两形态/发现链与快轮，两家挂载集成各 1）；
  ✅ cargo check 零告警；⬜ `test_real_otel_gemini`/`test_real_otel_qwen` 对账留待
  装机（清单 01-RESEARCH §13.3 增补项）；✅ 所有者 dev 验收通过（2026-09-23）

### M2-13 OpenClaw 适配器（多 agent 多库 SQLite 档＋zstd 事件流）✅（2026-09-24 代码完成，装机对账留待）

- 背景（源码级调研先行，详 01-RESEARCH §14；所有者拍板本机不装机）：openclaw/openclaw
  main 分支核实——**三处总纲外新知**：①每 agentId 一库（`<root>/agents/<id>/agent/`
  openclaw-agent.sqlite，多库枚举，session_key 跨库同名须带 agentId 消歧）；
  ②转录事件 ≥1KB 转 zstd 是常态（event_zstd BLOB＋event_utf8_bytes 长度校验，
  Rust 侧引入 zstd crate 解压）；③回合式一次性落盘（无 CC 式流式中间快照，
  usage 即每回合增量，无双计面）。状态根四候选：OPENCLAW_STATE_DIR（须存在）>
  ~/.openclaw > ~/.openclaw-<profile> > ~/.clawdbot 旧目录
- 内容：`collector/openclaw.rs` 新建——多库发现（agents 目录枚举，incognito 哨兵
  保留名自然排除）＋scan（session_nodes 90 天窗口，display_name>label 标题链，
  LEFT JOIN 当前窗口取模型列）＋collect（窗口 updated_at>水位-60s 选活跃窗＋
  per-window seq 水位只解析新增，重启全窗重放靠 `oc:{agent}:{key}:{seq}` 幂等键
  兜底，官方 assistantIdempotencyKey 存在时 `ocm:{key}` 优先）＋usage 四桶直取
  （input 落盘前已归一不含缓存，免拆分）＋stopReason=error→错误行（弱信号）/
  aborted→用户打断不计（对齐 ZCode cancelled）＋DirScan 快轮（每状态根一个，
  agents 限深 2 枚举 *.sqlite）＋进程匹配声明化；错误行 api_error 精确载体留待装机
- 涉及：src-tauri/src/collector/{openclaw,mod}.rs、state/service.rs（注册＋OpenClaw
  无 hooks 不入 HOOKS_AGENTS）、Cargo.toml（zstd 0.13）、src/shared/types.ts（
  AGENT_DEFS +1 openclaw 龙虾红 #ef4444）、docs/01-RESEARCH（§14 勘察节＋日志）
- 验证：✅ cargo test **78/78** 全绿（新增 7：状态根解析四候选/多库发现排除哨兵/
  scan 标题回退链与模型 join/端到端采集含 zstd 解压与官方幂等键/seq 水位增量续读/
  解码校验含长度不符防坏解压/解析边界含浮点取整与 provider 推断）；
  ✅ npm run build 通过（echarts chunk 警告为 M1-1 已知现状）；
  ⬜ `test_real_openclaw` 对账留待装机（清单 01-RESEARCH §14.3 八项）
- 依赖：无（独立适配器批次）

### M2-14 Hermes 适配器（state.db 累计快照重采档）✅（2026-09-24 代码完成，装机对账留待）

- 背景（源码级调研先行，详 01-RESEARCH §15；所有者拍板本机不装机）：
  NousResearch/hermes-agent main 分支核实——**四处总纲预想修正**：①「SqliteTail
  水位采 usage 行」不成立，**库内无逐调用流水表**（messages.token_count 仅单值
  整数），唯一四桶数据面是累计快照 → 采集走 session_model_usage 行重采＋自库
  「保留最大快照」幂等（rewind 不清零、增量路径单调、gateway absolute 只覆盖
  sessions 主行，快照单调安全）；②记账双通路（update_token_counts 唯一咽喉：
  CLI 增量累加/gateway absolute 覆盖主行；后台合并 writer 秒级延迟落盘）；
  ③session_model_usage.task 列即后台辅助调用维度（vision/压缩/标题生成）——
  直接映射 is_background（token 计入、次数不计，对齐 CC cost-state 裁定）；
  ④库内无逐调用错误载体（api_request_error 仅 hooks 面）→ 无错误行，错误纯启发式
- 内容：`collector/hermes.rs` 新建——状态根（HERMES_HOME env 目录须存在，env 与
  已有根同路径跳过保 tag 稳定＞Windows %LOCALAPPDATA%\hermes＞默认根/profiles/<名>
  命名 profile 枚举，tag=default/<名>/env 消歧）＋scan（sessions 表 90 天窗，
  标题链 title>display_name，cwd/model/billing_provider 直取，REAL 秒转毫秒）＋
  collect（last_seen 水位＋进程内高水位双层，幂等键 `hm:{tag}:{sid}:{model}:{task}:
  {provider}:{base}:{mode}` 六元组主键全拼）＋无 hooks 注入（Hermes 有 shell
  hooks 体系但为 YAML config.yaml＋consent allowlist 机制，注入成本高且 SQLite
  通道已覆盖，**列装机后增强档**，不入 HOOKS_AGENTS）＋快轮每 home 两个 File
  信号（state.db＋state.db-wal，WAL 主库 mtime 不动）＋进程匹配 cmd 含 hermes
  （装机核实）；前端 AGENT_DEFS +1（hermes 暗金 #eab308）
- 已知口径差（装机对账评估）：ts 取 last_seen（该累计行最后写入时刻），历史会话
  token 记账日压缩到末日；官方 usage_totals 口径排除子会话/空会话/归档，我们全收
- 涉及：src-tauri/src/collector/{hermes,mod}.rs、state/service.rs（注册＋不入
  HOOKS_AGENTS）、src/shared/types.ts（AGENT_DEFS +1）、
  docs/{01-RESEARCH,04-MULTI-AGENT-EXPANSION,03-TASKS}.md
- 验证：✅ cargo test **84/84** 全绿（新增 6：状态根解析含 env 去重/库发现/
  scan 标题链与秒转毫秒/端到端采集含后台行与 provider 回退/last_seen 水位增量
  含快照升级/快轮信号双文件）；✅ npm run build 通过（echarts chunk 警告为
  M1-1 已知现状）；✅ cargo check 零 hermes 告警；
  ⬜ `test_real_hermes` 对账留待装机（清单 01-RESEARCH §15.3 八项）
- 依赖：无（独立适配器批次）

## 待议区（看板外想法，不擅自实施）

- **零用量会话不在会话窗口显示**（M1-10 有意边界；**2026-09-21 所有者拍板收尾**）：
  会话查询沿用 usage_records 聚合口径（岛面板=文件扫描含零用量会话，两处数量天然不同），
  保持现状不改 LEFT JOIN；差异以交互引导消化——会话窗口标题右侧新增口径说明小字
  「仅统计产生过调用的会话」，悬浮 Tip 给出与岛面板口径差异的完整解释
  （所有者原则：数据口径不一致要么设计引导、要么保持一致，不能让用户猜）
- **报表页不补汇总级导出**（M1-10 拍板）：导出能力随会话明细整体迁往会话窗口；
  若日后需要"按范围汇总一行"的导出再议
- ~~CC 跨 tick 流式重复残留~~ → **已解决（M1-12，2026-09-21）**：幂等键升级 source_id
  ＋存量自动清空重建，实测活跃日 +27.8% 双计归零，详见 M1-12 与 01-RESEARCH §8
- **hook 事件文件无轮转**：%LOCALAPPDATA% 事件文件 append-only 无限增长，且每 10s
  tick 全量读取一次；长期运行需补轮转/截断策略
  → **部分过时（2026-09-21 核实）**：8MB 轮转与偏移增量读已随审查 1.2 落地
  （collector/hook_events.rs rotate_if_large），本条仅剩"轮转保留一代 .old"的现状记录
- **CC 采集全量重读**：每 tick 全量重读所有 mtime 新于水位的转录文件；
  scan_sessions 对 90 天内全部转录读 8KB 头后才截断 100——转录量大时改文件内
  偏移增量（即原 02-DESIGN §2.1 的文件内偏移方案）
  → **部分过时（2026-09-21 核实）**：per-file 字节偏移增量读已随审查 1.3 落地，
  仅剩 scan_sessions 的 8KB 头读取（cwd 缓存已按 mtime 缓解）
- **sessions 表不在清理范围**：每会话一行缓慢累积，长期可考虑随数据保留周期清理
- **CC cost-state 后台量已补采（M1-12）**：差值行 is_background=1，token 计入、
  次数不计；后续若需要"后台调用独立维度报表"再议
- **CC 启发式状态的 mtime 噪声（M1-12 遗留观察）**：未装 hooks 时 last_activity_at 取
  文件 mtime，ai-title/file-history 等非对话行追加会推高造成"假 working"（装 hooks 的
  用户不受影响）；后续可改用自库最近调用时间兜底
- **WT 中 Claude Code 卡片跳转未命中**（T10 遗留）：ZCode ✅/标题含路径的场景理论 ✅，
  但所有者环境（Windows Terminal + PowerShell 7）进程链匹配未命中。下次调试线索：
  ①打印 WT 进程树（pwsh 的父链是 WindowsTerminal.exe 还是经 OpenConsole/conhost 中转）；
  ②确认 sysinfo 能否读到 WT 子进程 cmdline（UWP 权限）；③考虑改用窗口类名
  （WindowsTerminal 的类 CASCADIA_HOSTING_WINDOW_CLASS）兜底。
  **2026-09-16 晚更新**：project_dir 已改为转录 cwd 真实路径（R2），标题匹配①②不再是必败；
  find_session_window 未命中时向 stderr 打印窗口清单与候选 PID（R14），dev 控制台可见

## 交接锚点

- 任何 Agent 接手：先读 [HANDOFF.md](HANDOFF.md)（当前进度/坑/下一步），再从最靠前的 ⬜/🟨 任务继续
- M0 验收标准全文见 [00-REQUIREMENTS](00-REQUIREMENTS.md) 维度⑩
