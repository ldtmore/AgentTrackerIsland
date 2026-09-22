# AgentTrackerIsland 调研报告

> 状态：**调研中**（阶段 2） | 开始：2026-09-16
> 目的：按 [00-REQUIREMENTS.md](00-REQUIREMENTS.md) 逐项找轮子，产出 🟢直接依赖 / 🟡借鉴实现 / 🔴必须自研 三分清单
> 注：早期竞品调研（2026-09-16 第一轮）存档于 E:\AIAgentTemp\AgentTracker-research\（2026-09-17 项目更名前存档，路径保留原名）

## 1. ZCode 本机数据勘察 ✅（2026-09-16，决定性成果）

### 结论：ZCode 适配器可**纯只读**实现，无需 hooks，数据齐全度超过 Claude Code

| 数据源 | 路径 | 内容与价值 |
|---|---|---|
| **SQLite 库** | `~\.zcode\cli\db\db.sqlite`(WAL) | 核心数据源。关键表：`model_usage`（每模型调用一行，496 行实测）、`turn_usage`（82）、`tool_usage`（462）、`session`（会话元数据） |
| 模型 IO 转录 | `~\.zcode\cli\rollout\model-io-sess_*.jsonl` | 完整请求/响应体（含 headers），可作为 SQLite 的补充校验源 |
| 套餐缓存 | `~\.zcode\v2\coding-plan-cache.json` | ZCode 原生内置 GLM 套餐支持：`builtin:bigmodel-coding-plan`（国内）/`builtin:zai-coding-plan`（国际）等 4 个套餐条目 → 证实 GLM 查询链路成熟 |
| 桌面设置 | `~\.zcode\v2\setting.json` | 仅 UI 偏好，无关采集 |

### `model_usage` 表关键字段（实测）

- **用量**：`input_tokens / output_tokens / reasoning_tokens / cache_creation_input_tokens / cache_read_input_tokens / computed_total_tokens`
- **标识**：`provider_id / model_id / agent / mode / task_type / session_id / turn_id`
- **性能**：`duration_ms / time_to_first_token_ms`（CC Switch 式时长统计的现成数据！）
- **状态/错误**（状态监控直接可用）：`status / error_type / error_code / error_message / retry_count / cancelled_by_user / context_exceeded`

### 采集方案定论（写入 02-DESIGN 的依据）

- 🟢 直接依赖：以 `node:sqlite`/`rusqlite` **只读打开** db.sqlite（WAL 模式支持并发读，不干扰 ZCode 运行）
- 🟢 会话状态：由 `model_usage.started_at/completed_at` + `session.time_updated` 推断（工作/空闲），错误由 `error_type` 判定
- 🟢 增量采集：按 `model_usage.id`/`started_at` 水位增量拉取，天然满足红线③（顺序无关/回溯补录）
- 🔴 需自研：仅"水位管理与聚合入库"薄层
- ⚠️ 风险：ZCode 升级可能改 Schema → 解耦层容忍未知列，按列名白名单读取

## 2. Claude Code 数据源（已验证）

- `~\.claude\projects\**\*.jsonl`：assistant 消息含 `message.model`(glm-5.3)+ `message.usage`(input/output/cache_read/cache_creation + thinking)
- hooks 机制：`PreToolUse/PostToolUse/Stop/Notification/SessionStart/SessionEnd` 等，`~\.claude\settings.json` 配置（所有者已在用 Notification/Stop hook，链路已验证）
- 对账基准：ccusage(18.5k⭐，Rust)

## 3. GLM Coding Plan 额度（已验证）

- 官方 Monitor API：`GET {open.bigmodel.cn|api.z.ai}/api/monitor/usage/quota/limit`，Header `Authorization:<key>`（无 Bearer,key=ANTHROPIC_AUTH_TOKEN 同值）→ 5h%/周%/套餐档/重置时间
- 积分制：5h 窗口动态刷新（消耗起 5h 后重置）；周窗口按下单日 7 天周期；glm-5.3/glm-5-turbo 高峰期（工作日 14:00–18:00 UTC+8）×3 系数
- 参考实现：glm-quota-line（33⭐）、ecerutti/glm-usage-monitor（接口三件套已列明）

## 4. Tauri 生态轮子盘点 ✅（2026-09-16）

| 需求 | 轮子 | 结论 |
|---|---|---|
| 文件监听 | notify-rs/notify（3452⭐，2026-09 仍活跃） | 🟢 直接依赖 |
| SQLite | rusqlite(bundled feature) | 🟢 直接依赖（自库读写 + ZCode 库只读均用它） |
| 毛玻璃 | tauri-apps/window-vibrancy（1037⭐，活跃；Acrylic/Mica） | 🟢 直接依赖 |
| 托盘/全局快捷键 | Tauri 2 内置（tray-icon feature） | 🟢 直接依赖 |
| 开机自启 | tauri-plugin-autostart（官方 plugins-workspace，1815⭐） | 🟢 直接依赖（M0 预留接口，默认关） |
| 置顶/无边框/透明窗 | Tauri 2 内置（always_on_top/decorations/transparent） | 🟢 直接依赖 |
| 窗口激活（跳转） | Windows SetForegroundWindow(windows-rs crate) | 🟢 直接依赖 |
| 图表 | ECharts（M1 报表页） | 🟢 直接依赖 |

## 5. 灵动岛 UI 实现参考 ✅（2026-09-16）

- 🔴 **Tauri 生态无现成灵动岛项目**（gh 搜索 "tauri island"/"tauri notch" 零结果）→ 岛 UI 自建
- 🟡 交互设计借鉴：open-island-windows（C#，19⭐）——顶部贴边收起/hover 展开/点击会话卡的交互范本（借鉴行为，不抄码）
- 🟡 视觉借鉴：iOS 灵动岛经典胶囊形态 + DynamicWin（599⭐）的 Windows 适配经验

## 6. Claude Code hooks 协议 ✅ 策略变更（2026-09-16）

- 官方文档站（code.claude.com / docs.anthropic.com）本机网络不可达（直连与代理均超时）
- **决定：hooks stdin 细节改为实施期实测验证**——所有者本机已配置 Notification/Stop hooks（结构：matcher/hooks[type=command,command,timeout,async] 已见），T6 任务先写诊断 hook 打印真实 stdin JSON 再定协议，比文档更可靠

## 7. GLM Monitor API 响应格式 ✅（2026-09-16，源码级确认）

来自 glm-quota-line 源码（src/core/quota/fetch.js + parse.js，已存 E:\AIAgentTemp\AgentTracker-research\）：

- 请求：`GET {base}/api/monitor/usage/quota/limit`，Header `Authorization: <裸key>`（无 Bearer）、`Accept: application/json`
- 响应字段（多口径容错）：`usage / remaining / currentValue / total / percentage(历史遗留字段=已用%) / nextResetTime(毫秒时间戳)`
- 限流判定正则（429 场景）：`rate limit|too many requests|限流|频率|过于频繁|稍后再试`
- token 级用量（报表用）：`/api/monitor/usage/model-usage?startTime&endTime`(ecerutti README)

## 8. T4 对账记录（2026-09-16，重要口径结论）

对账三方（同一台机器、同一数据）：

| 工具 | input | output | cache_read | total | 口径 |
|---|---|---|---|---|---|
| **AgentTrackerIsland（T4 实现）** | 1,295,911 | 379,214 | 26,300,032 | 27,975,157 | assistant 行，messageId（+requestId）去重保留最大快照 |
| ccusage（Rust 版） | 1,495,477 | 389,518 | 26,572,544 | 28,457,539 | 同上去重 + 额外纳入非 assistant 源 |
| better-ccusage（TS 版） | 75,329,127 | 884,293 | 98,125,376 | 174,338,796 | 多源（Claude Code+ZCode 混合），不可直接比 |

**结论与依据**：

1. 本机 1514 条 assistant 行实测**无一条带 requestId**（0/1514），组合键退化为 messageId，
   我们与 ccusage 在 assistant 行源上的口径完全一致（better-ccusage 的 `createUniqueHash` 同款逻辑）
2. 残差 1.7%（482K）来源：本机数据存在 **`cost-state` 行类型**（56 行，会话级模型用量累计快照，
   含 glm-5.3-flash 等非 assistant 路径的用量），ccusage Rust 新版将其纳入；两个参考工具互不一致
   证明"标准口径"本身不唯一
3. **本地解析只是近似，供应商侧数字才是最终裁判**——T5 接入 GLM Monitor API 后，
   用官方用量对账作为 A3 验收的黄金标准
4. 关键去重知识（后续维护必读）：JSONL 同一 assistant 消息平均重复 ~3 次（流式快照/会话恢复复制），
   按 messageId 去重是底线；`<synthetic>` 模型行是本地合成消息，usage 为 0，过滤

### §8.1 M1-12 双计实证与根治（2026-09-21，重要）

按天对账（自库 vs 现存转录文件"按 message.id 去重保留最大快照"口径，本地时区分组）：

| 日期 | 修复前自库 | 真实口径 | 误差 | 结论 |
|---|---|---|---|---|
| 08-30 ~ 09-16 各天 | — | — | 0.0% | 历史数据是工具未运行期间写入，启动后一轮全量回溯＋调用内 message.id 去重，恰好无误 |
| **09-21（活跃日）** | 889,947 | **696,302** | **+27.8%** | 工具运行期间同消息复制行（timestamp 各异 1~10s）跨采集轮次分裂入库 → 实时双计 |

1. **多行快照形态**（实测 41 文件 1605 行）：492/534 条消息多行，其中 491 条各行 usage
   **完全相同**（纯复制）、timestamp 首末差 1~10s 占 334 条——不是流式累积（应递增）也不是
   重试独立（应回落），"保留最大快照"口径不受影响，问题只在幂等键 `(sid,ts,model)` 不含
   消息身份、跨轮分裂时各自成行
2. **cost-state 补采量化**：72 行、62 行带非空 `modelUsage`（camelCase 字段、模型名带
   `[1m]` 后缀须归一化、**无 ISO timestamp 只有毫秒 startTime**）；按（会话×归一化模型）
   取最大累计快照，超出 assistant 已计的部分 = **1,326,495 token（4.55%）** 为后台调用
   真实缺口（旧对账 1.7% 低估：当时未做模型名归一化对齐）
3. **根治方案与验证**：迁移 0003 幂等键升级 `(agent, session_id, source_id)`＋存量自动清空
   重建；端到端临时库重建后今日 CC = 696,302 与现存文件真实口径**分毫不差**，
   总量 30.48M ≈ assistant 29.1M＋后台 1.33M
4. **顺带实测结论**：ZCode 源库 `model_usage.id` 为 TEXT 型（rusqlite 按 i64 读全部行
   解析失败）；`cancelled_by_user=1` 的行 error_type='cancelled'（用户主动取消，不再计错误）；
   按天对账时自库按本地时区分组、转录行 timestamp 是 UTC——跨零点会错天，对账脚本须统一时区

## 9. 报表页技术选型 ✅（2026-09-17，M1-1 开工前调研）

- 🟢 **直接依赖 echarts@6.1.0**（npm 最新稳定，PLAN 预留的 6.x 兑现）。不引入
  echarts-for-react 包装库（3.0.6 仍在维护，但它只做 init/dispose/resize 三件事，
  手写标准配方约 30 行即可，少一个依赖；社区共识两种方式等价）
- 🟡 **React 薄封装配方**（社区标准）：挂载 echarts.init → 卸载 chart.dispose()*
  → 容器 ResizeObserver 触发 chart.resize() → option 变更 getInstanceByDom().setOption()。
  *不 dispose 泄漏 zrender 实例、不 resize 图表不填容器，两大新手坑
- 🟢 **按需引入**（官方 handbook）：`echarts/core` + `echarts/charts`{Line,Bar,Pie,Heatmap}Chart
  + `echarts/components`{Grid/Tooltip/Legend/VisualMap} + CanvasRenderer,echarts.use() 注册
- 🟢 **代码分割**：报表组件 React.lazy 动态导入——Tauri 各窗口共享同一份前端产物，
  echarts 只有打开报表窗口才加载，岛窗口体积/启动内存不受影响
- Tauri WebView2 兼容性：常规 Canvas 渲染无已知系统性问题（低风险，以屏幕实测为准）

---

## 10. Codex CLI 数据源勘察（2026-09-17，M1-3 第一步：源码级调研）

基于 openai/codex 主分支源码（gh api + raw 拉取，基线 2026-09）：

| 项 | 结论 |
|---|---|
| 会话文件 | `~/.codex/sessions/rollout-<时间戳>-<thread_id>.jsonl`（append-only；另有归档子目录） |
| 行结构 | JSONL：`{timestamp, type, payload}`；type ∈ session_meta / response_item / event_msg / compacted 等 |
| token 用量 | event_msg → `TokenCount` 事件：TokenUsage = input_tokens / cached_input_tokens / cache_write_input_tokens / output_tokens / reasoning_output_tokens / total_tokens；并有 turn/thread 级聚合 |
| 会话元数据 | `session_meta`（SessionMeta:thread 级 id/cwd 等，跨 revert 稳定） |
| ⚠️ 新变数 | 新版出现 codex-state **SQLite 状态库**（SqliteConfig，线程列表可从状态库读）——JSONL 仍是 replay 权威源；状态库或为 M2+ 更优读取面，待实测 |
| 适配器方案 | 与 Claude Code 适配器同构：扫描 sessions/*.jsonl → 逐行解析 → token_count 事件按 response_id 幂等入库；thread_id 作 session id；session_meta 取 cwd |
| 🔴 待实测 | ①token_count 在 JSONL 的确切 payload 字段大小写/嵌套；②session_meta 是否含 model/cwd；③SQLite 状态库的实际角色（版本相关）——**需一台装有 Codex 的机器取真实样本**，所有者尚未安装 |

适配器实现（实现 AgentAdapter trait）待所有者实际使用 Codex 后进行（无真实数据不可验证）。

---

## 11. 各 Agent 数据面勘察（P1，2026-09-23，M2-6/M2-7 开工前源码级核实）

> 本节承接 04-EXPANSION §1.2 矩阵的逐家细节；核实方式 = GitHub 源码逐文件核对
> （serde 属性级别）；**所有者本机未装两家 CLI，真实样本对账（test_real_codex /
> test_real_kimi，均 #[ignore]）留待装机后补跑**，解析器对未知格式宽松忽略。

### 11.1 Codex CLI（openai/codex，main 分支，2026-09-23 核实）

| 项 | 结论（源码确证） |
|---|---|
| 会话文件 | `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<UTC时间戳>-<thread_uuid>.jsonl`（本地日期分区）；revert 变体 `..._<rollout_id>` 归并回主 thread；归档在 `archived_sessions/`（不采） |
| CODEX_HOME | 重定向目标必须已存在且是目录（官方 find_codex_home 同规则），异常回落 `~/.codex` |
| 行 envelope | `{timestamp: RFC3339 毫秒字符串, ordinal?, type, payload}`（item flatten 平铺）；type 12 种（session_meta/response_item/event_msg/turn_context/token_usage_record/compacted 等） |
| TokenUsage | snake_case 无 rename：`input_tokens / cached_input_tokens / cache_write_input_tokens / output_tokens / reasoning_output_tokens / total_tokens` |
| 用量双通路 | ①`token_usage_record` 行（新版权威记账，payload.response_id 作幂等键）②`event_msg→token_count`（info.last_token_usage 单次量 / total_token_usage 会话累计）。**适配器取①为主，①缺失退②，同轮增量互斥防双计**；total_token_usage 是累计快照绝不可按行入库 |
| ⚠️ 缓存口径 | OpenAI 的 cached_input_tokens 是 input_tokens **子集**（Anthropic 的 cache_read 是独立分项）——按四项互斥口径拆分 `input -= cached`（装机对账验证点） |
| 模型名 | session_meta **无 model**；随 `turn_context` 行逐 turn 更新（per-file 缓存承接增量） |
| hooks | config.toml 顶层 `hooks` 段（HooksToml：12 事件 flatten + state）；形状 `[[hooks.<Event>]]`（Vec<MatcherGroup>）→ `[[hooks.<Event>.hooks]]`（type="command"/command/commandWindows/timeout 秒/async/statusMessage，deny_unknown_fields）；事件 PascalCase ×12（CC 同名超集 + PermissionRequest/PreCompact/PostCompact/SubagentStart/SubagentStop/Interrupt）；stdin snake_case JSON（session_id/hook_event_name/cwd/transcript_path）；命令经 cmd /C 执行；user 级 = `$CODEX_HOME/config.toml` |
| 进程 | npm 包 `@openai/codex` 只是启动器，常驻进程为原生 `codex.exe`（win32-x64 平台包） |
| legacy notify | config.toml `notify = [argv...]`，kebab-case JSON 追加为最后一个 argv（`agent-turn-complete`）；hooks 的降级补充，暂不接入 |

### 11.2 Kimi Code CLI（MoonshotAI/kimi-code v2.0.2，2026-09-23 核实）

> ⚠️ **纠正 04-EXPANSION §1.2 的旧版记录**：原记录（`~/.kimi/sessions/<md5>/…/
> {context.jsonl, wire.jsonl, state.json}`、13 hooks、KIMI_SHARE_DIR）是旧版
> Python kimi-cli（已归档）的格式。当前主线为 TypeScript 的 **kimi-code**，按新版实现。

| 项 | 结论（源码+文档确证） |
|---|---|
| 数据根 | `~/.kimi-code`（`KIMI_CODE_HOME` 重定向整个数据根；旧 KIMI_SHARE_DIR 新版运行时完全不读，仅旧数据迁移时参考） |
| 会话结构 | `sessions/<wd_key>/<sid>/{state.json, agents/<agentId>/wire.jsonl, tasks/…}`；wd_key = `wd_<basename_slug>_<sha256前12>`（不可逆，cwd 明文在 state.json 与 `session_index.jsonl`）；context.jsonl 已不存在 |
| 用量行 | wire.jsonl `type:"usage.record"`（durable 落盘），字段 **camelCase**：`{agentId, model, usage:{inputOther, output, inputCacheRead, inputCacheCreation}, usageScope?('session'\|'turn'), time: Unix 毫秒}`；**无 reasoning 独立字段** |
| 幂等键 | wire 行无官方 id → source_id = 内容指纹 `kr:{agentId}:{time}:{四项}`（同毫秒同用量碰撞合并，误差可忽略）；⚠️ usageScope 语义未实证（若 session 行是累计快照会双计）——按「每请求一行」假设实施，装机对账验证 |
| 会话元数据 | state.json（SessionMeta v2，camelCase）：title/titleKind/createdAt/updatedAt(ms)/cwd/archived/lastTurnReason 等；无 running/idle 显式状态字段 |
| hooks | `~/.kimi-code/config.toml` 顶层 `[[hooks]]` 数组，**仅允许 event/matcher/command/timeout 四字段**（多余字段配置加载失败）；事件 ×20（CC 同名 + TurnStarted/UserPromptQueued/PermissionRequest/PermissionResult/PostToolUseFailure/StopFailure/Interrupt/TaskStarted/PreCompact/PostCompact/SessionHeartbeat）；stdin snake_case（hook_event_name/session_id/session_title/client_type/cwd，无 transcript_path）；失败类事件（StopFailure/PostToolUseFailure）带 errorType/errorMessage——桥脚本归入 message 透传 |
| 进程 | 官方安装 = `kimi.exe`（Node SEA 单二进制，无需 Node）；npm 安装 = node shim（命令行含 `@moonshot-ai/kimi-code`）；`kimi-legacy` 是旧版改名，进程匹配排除 |
| 状态精度 | working：TurnStarted/PreToolUse 等 hooks + wire 追加 mtime；waiting：PermissionRequest/Notification；**error：StopFailure/PostToolUseFailure（信号最精确，喂 sig.last_failure）**；SessionHeartbeat 每 60s（仅配置了才有） |

### 11.3 待装机核实清单（装机后回写本节并跑 test_real_*）

1. Codex：token_usage_record 与 token_count 在真实 rollout 的实际共存形态（互斥策略验证）；缓存拆分口径与官方统计对账；hooks 注入后真实触发链
2. Kimi：usageScope='session' 行是单请求还是累计快照（双计验证）；wire 行 time 缺失率；hooks 20 事件真实 stdin 样本；`~/.kimi-code` 实际落盘结构核对
3. 两家可达状态集按 §2.7（04-EXPANSION）回写

---

## 调研日志

| 日期 | 进展 |
|---|---|
| 2026-09-16 | §1 ZCode 勘察完成（决定性）；§2/§3 基于早期调研归档 |
| 2026-09-16 | §4 轮子盘点完成（全🟢）；§5 确认 Tauri 无现成岛→自建；§6 hooks 改实测策略；§7 GLM 响应格式源码级确认——**阶段 2 调研收官** |
| 2026-09-17 | §9 报表页选型完成（echarts@6.1.0 直用+按需引入+lazy 分割），M1-1 开工 |
| 2026-09-17 | §10 Codex CLI 源码级勘察完成（sessions/*.jsonl + TokenCount 事件），适配器待实测后开发，M1-3 第一步收官 |
| 2026-09-23 | §11 新增（P1 勘察节）：Codex/Kimi Code 两家源码级核实完成（serde 属性级）；Kimi 记录纠正为新版 kimi-code；M2-6/M2-7 按此实施，真实对账留待装机 |
