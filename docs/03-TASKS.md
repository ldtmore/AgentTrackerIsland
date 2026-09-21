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

### 已划出 M1（2026-09-17 所有者拍板）

- Anthropic 官方订阅额度 → 移回 M2 愿景池（所有者无官方订阅）
- NSIS 安装包 → 归正式发布阶段（WORKFLOW 构建打包纪律）
- 悬浮球形态 → 不做（M1-6 贴边自动隐藏已覆盖其价值，详见 M1-5 条目）

## 待议区（看板外想法，不擅自实施）

- **零用量会话不在会话窗口显示**（M1-10 有意边界）：会话查询沿用 usage_records
  INNER JOIN（与原报表同口径），装了 hook 但从未产生模型调用的会话不可见；
  改 LEFT JOIN 需把时间过滤挪进 ON 子句，真实遇到此类会话再议
- **报表页不补汇总级导出**（M1-10 拍板）：导出能力随会话明细整体迁往会话窗口；
  若日后需要"按范围汇总一行"的导出再议
- **CC 跨 tick 流式重复残留**（M1-7 P2 余项）：usage_records 幂等键不含 messageId，
  同一消息的多条流式快照行若时间戳不同仍会入库两行（同键取最大快照场景已修复）。
  影响用量准确度，需取真实增量样本测影响量级再定方案（加 message_id 列需迁移 0002）
- **hook 事件文件无轮转**：%LOCALAPPDATA% 事件文件 append-only 无限增长，且每 10s
  tick 全量读取一次；长期运行需补轮转/截断策略
- **CC 采集全量重读**：每 tick 全量重读所有 mtime 新于水位的转录文件；
  scan_sessions 对 90 天内全部转录读 8KB 头后才截断 100——转录量大时改文件内
  偏移增量（即原 02-DESIGN §2.1 的文件内偏移方案）
- **sessions 表不在清理范围**：每会话一行缓慢累积，长期可考虑随数据保留周期清理
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
