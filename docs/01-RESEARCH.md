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

## 12. 各 Agent 数据面勘察（P2，2026-09-23，M2-10 开工前源码级核实）

> 本节承接 04-EXPANSION §1.2 矩阵的逐家细节；核实方式 = GitHub 源码逐文件核对
> （zod/effect Schema 字段级，drizzle 表列级）；**所有者本机未装两家 CLI，真实样本
> 对账（test_real_opencode / test_real_mimo，均 #[ignore]）留待装机后补跑**。

### ⚠️ 纠正 04-EXPANSION §1.2 的旧记录

原记录「OpenCode = 本地 storage（逐实体 JSON 文件）」「MiMo 项目配置目录 `.mimocode`、
环境变量 `MIMOCODE_CONFIG_DIR`」均已过时：OpenCode 主存储已迁移 SQLite（v2 存储层
`@opencode/v2/storage/Database`），JSON storage（`data/storage/`）仅剩会话回滚、导入
等边缘用途，旧数据由 `json-migration` 自动迁移；MiMo 数据根重定向是 `MIMOCODE_HOME`
（shared/src/global.ts 源码确证）。通道优先级随之反转：**只读 SQLite 为主通道
（零用户配置，红线④），SSE 降为 opt-in 实时增强档（M2-10b，装机后实施）**。

### 12.1 OpenCode（anomalyco/opencode v1.18.32，2026-09-23 核实）

| 项 | 结论（源码确证） |
|---|---|
| 仓库 | 2026 年由 sst/opencode 迁至 **anomalyco/opencode**（旧地址自动跳转）；最新 v1.18.32（2026-09-21 发布）；monorepo 大拆分（core/schema/server/protocol 等独立包） |
| 主存储 | SQLite v2：`~/.local/share/opencode/opencode.db`（稳定渠道）或 `opencode-<channel>.db`；**Windows 无特判**——xdg-basedir npm 包只认 `XDG_DATA_HOME` 环境变量，缺省直拼 `~/.local/share`（global.ts 源码确证） |
| session 表 | drizzle `session`：id/project_id/directory/title/agent/version + `model` JSON 列 `{id,providerID,variant?}` + **cost 与 tokens 五项会话级累计列**（`tokens_input/output/reasoning/cache_read/cache_write`，迁移 `20260510033149_session_usage` 从 message 行聚合而来——聚合缓存，非真源）+ Timestamps（time_created/time_updated，Unix 毫秒） |
| message 表 | drizzle `message`：id（`msg_` 前缀，ascending 有序）/session_id/`data` JSON 列 + Timestamps；assistant 行 data：`{tokens:{input,output,reasoning,cache:{read,write}}, cost, model:{providerID,id}, agent, error?, finish?, time:{created,completed?}}`（packages/schema/src/session-message.ts）；time.created 为 ISO UTC 字符串（DateTimeUtcFromMillis 编码侧=毫秒） |
| 用量口径 | cache.read/write 是**独立分项**（Anthropic 语义，非 OpenAI 子集）→ 四项互斥直取，无需拆分；幂等键 `oc:msg_{message_id}` 天然唯一 |
| 错误信号 | assistant 行 `data.error` 非空 = 本步失败（UnknownError{message}）→ 填 UsageRow.error_type 走现有 recent_error 链路 |
| SSE | `opencode serve` 默认绑 **127.0.0.1、端口 0（随机）**；`GET /event`（text/event-stream），事件包装 `{id,type,properties}`；**`session.next.step.ended` 直接带 cost＋tokens 五项**（durable v2）、`step.failed` 带 error、`prompted`=回合起点；10s 心跳 `server.heartbeat`；按 instance.directory 过滤；Basic 鉴权可选（`OPENCODE_SERVER_PASSWORD` 未设即无鉴权，server/auth.ts） |
| 端口发现 | **无固定端口、无端口落盘文件**（serve 打印 stdout；TUI 内嵌 server worker 按需起，cli/tui/worker.ts；桌面 sidecar 同）——10b 的核心装机核实点 |
| hooks | 无 CC 式 hooks 体系（有 plugin 体系，packages/plugin）→ 不接入，SQLite 通道+进程探测已覆盖 |
| 进程 | npm `opencode` 启动器；常驻进程 exe 名装机核实 |

### 12.2 MiMo Code（XiaomiMiMo/MiMo-Code v0.1.15，2026-09-23 核实）

| 项 | 结论（源码确证） |
|---|---|
| 内核关系 | OpenCode fork 确证：`packages/opencode` 同构；fork 基线在「SQLite 存储迁移之后、core 包大拆分之前」（有 storage/db.ts 无独立 core 包） |
| 数据根 | `MIMOCODE_HOME`（须绝对路径，shared/src/global.ts）→ `{data,cache,config,state}` 四子目录；缺省 XDG → Windows `~/.local/share/mimocode`；⚠️ 纠正总纲旧记录（`.mimocode`/`MIMOCODE_CONFIG_DIR` 当前源码不存在） |
| db | `data/mimocode.db`（或 `mimocode-<channel>.db`；环境变量 `MIMOCODE_DB` 可指定文件名，db.ts:33-41） |
| session 表 | 同构 OpenCode 但**无 cost/tokens 聚合列**（fork 基线早于 `20260510` 迁移）→ 用量必须逐行取 message；directory/title/agent/model 字段同构 |
| message 表 | `data` JSON 列同构；assistant 行字段平铺（zod schema，session/message-v2.ts）：`{modelID, providerID, cost, tokens:{total?,input,output,reasoning,cache:{read,write}}, parentID, error?, path:{cwd,root}, time:{created,completed?}}`；另有 part 表 `step-finish` 部件带同构 tokens（不采，message 行已足） |
| 幂等键 | `mc:msg_{message_id}`（MessageID 同 `msg_` 前缀体系） |
| SSE | 同款 `mimo serve`（bin 名 `mimo`，npm `@mimo-ai/cli`）+ `/event` + `SERVER_PASSWORD` 鉴权；10b 一并核实 |
| 只读背书 | 官方自带 `storage/read-sqlite.ts` 只读读取层，注释明说供外部读 `opencode.db`——佐证只读 SQLite 通道是官方认可的外部观测姿势 |
| 进程 | `mimo`（npm shim）/官方二进制 exe 名装机核实 |

### 12.3 待装机核实清单（装机后回写本节并跑 test_real_*）

1. Windows 实际落盘：`~\.local\share\opencode\opencode.db` / `~\.local\share\mimocode\mimocode.db`（xdg 推断确证位）
2. 真实 message data JSON 样本对账（分项 token 与官方 /stats 对齐；`oc:`/`mc:` 幂等重采零重复）
3. OpenCode session 累计列 vs message 行聚合交叉对账（双口径互验，漂移早期报警）
4. M2-10b 前提：serve 端口发现（stdout 解析？TUI 内嵌 server 是否可订阅）、`/event` 真实事件流样本、directory 过滤行为
5. 进程 exe 名（opencode.exe / mimo.exe / mimocode.exe）
6. 两家可达状态集按 §2.7（04-EXPANSION）回写

## 13. 各 Agent 数据面勘察（P2，2026-09-23，M2-11 开工前源码级核实）

> 本节承接 04-EXPANSION §1.2 矩阵的逐家细节；核实方式 = GitHub 源码逐文件核对
> （zod schema 字段级＋hookRegistry 执行器语义级）；**所有者本机未装两家 CLI，
> 真实样本对账（test_real_gemini / test_real_qwen，均 #[ignore]）留待装机后补跑**。
> 两轮调研：先数据面（会话落盘/token 口径），后 hooks 字段级协议（所有者拍板
> M2-11 扩大范围含 hooks 注入）。

### ⚠️ 纠正 04-EXPANSION §1.2 的旧记录（三处）

1. **「Qwen 与 Gemini 同一适配器换根参数直接覆盖」不成立**：Qwen Code fork 基线
   是 gemini-cli **v0.8.2**，官方 README 明示**自 v0.1 起停止同步、独立演进**
   （现 v0.24.4 vs 上游 v0.60.0）——两家落盘结构已大幅分叉（目录/文件名/记录
   schema/控制行机制全不同）。M2-11 实施为**两份独立适配器**，仅共享 token 拆分
   口径小函数（强行 Family 参数化违反 YAGNI）。
2. **Gemini「无 hooks」不成立**：现有 **11 种 CC 式 hooks**（settings.json `hooks`
   键，PascalCase 事件名）；Qwen 亦有 **22 种**（几乎全 CC 同名）。
3. **Gemini chats 已 JSONL 化**：旧记录「单 JSON 对象」过时——现 `session-*.jsonl`
   追加写入，且 token 元数据后到时**同 id 消息整条重 append**；目录名由 sha256
   hash 改为 `~/.gemini/projects.json` 注册表分配的可读 slug（旧目录自动迁移）。

### 13.1 Gemini CLI（google-gemini/gemini-cli v0.60.0，2026-09-23 核实）

| 项 | 结论（源码确证） |
|---|---|
| 会话落盘 | `~/.gemini/tmp/<项目slug>/chats/session-<本地时间>-<id前8>.jsonl`（JSONL 追加）；子代理嵌套 `chats/<父sessionId>/<子sessionId>.jsonl`（本期不采，装机核实） |
| 行结构 | 首行 metadata `{sessionId, projectHash, startTime, lastUpdated, kind, directories, summary?}`＋消息行 `{id, timestamp, type(user/gemini/info/error/warning), content, displayContent?, model?, thoughts?, tokens?, toolCalls?}`＋控制行 `{$set:…}`（标题等，可带 messages 全量数组）/`{$rewindTo: id}`（回滚） |
| token 落盘 | `tokens:{input,output,cached,thoughts?,tool?,total}`（TokensSummary，input=promptTokenCount 原值）；**官方 /stats 口径 input = prompt − cached**（uiTelemetry.ts 源码确证，cached ⊆ prompt OpenAI 语义）→ 入库前拆分；thoughts 进 reasoning |
| error 信号 | type:"error" 消息行（displayContent/content 文本）→ UsageRow.error_type 走 recent_error 链路 |
| hooks | settings.json `hooks` 键：11 事件 PascalCase → `[{matcher?, sequential?, hooks:[{type:"command",command,name?,description?,timeout?,env?}]}]`；**无 async 字段**（同步执行，每次 spawn 一个 PowerShell≈几百 ms）；timeout 单位**毫秒**（默认 60000，超时 taskkill 强杀进程树）；Windows 一律经 PowerShell（`-NoProfile -NonInteractive -Command`）；校验宽松（多余字段无害，zod passthrough）；matcher：BeforeTool/AfterTool 按工具名正则、SessionStart/SessionEnd/PreCompress 按 trigger 精确、其余忽略；exit 0＋stdout/stderr 全空 = 零副作用最干净形态；CLI 自带 `name:command` 去重；⚠️ settings.json 支持 JSONC 注释（stripJsonComments） |
| hooks stdin | 基础 `{session_id, transcript_path, cwd, hook_event_name, timestamp}`（snake_case）；BeforeAgent＋prompt；AfterAgent＋prompt/prompt_response/stop_hook_active；Before/AfterTool＋tool_name/tool_input(/tool_response)；Notification＋notification_type("ToolPermission")/message |
| 注入事件集（7） | SessionStart/BeforeAgent/BeforeTool/AfterTool/Notification/AfterAgent/SessionEnd（语义对齐 CC 7 事件；BeforeModel/AfterModel/BeforeToolSelection/PreCompress 无增量价值不注入——同步 hook 有真实 spawn 开销）；条目 `{hooks:[{type,command,timeout:5000}]}`（无 async，毫秒） |
| OTel outfile | `telemetry.outfile`（env GEMINI_TELEMETRY_OUTFILE）确证存在，默认 `enabled=false`；outfile 时 span/log/metric 三类**混写同一文件**，`safeJsonStringify(data,2)+'\n'` 追加——**pretty 多行 JSON 流，非单行 JSONL**（解析需括号配平）；log 记录属性直接带全部 6 项原始计数（input_token_count 等）；metrics 每 10s 累计导出；`logPrompts` 默认 true（outfile 会带 prompt 文本——若启用只取数字字段，设置页引导关 logPrompts）；默认 otlpEndpoint `http://localhost:4317` grpc |
| 重定向 | `GEMINI_CLI_HOME` 整根重定向（paths.ts homedir() 源码确证） |
| 进程 | npm `@google/gemini-cli`，bin=dist/index.js（ESM）；Windows 进程 = node.exe，命令行含 `@google\gemini-cli\dist\index.js`（name 无特征，cmd_keywords 匹配 gemini-cli） |
| 状态可达 | working=chats mtime 启发式（10a 档）＋hooks 7 事件（11 档）；waiting=Notification(ToolPermission)（hooks 注入后）；error=error 消息行；idle/offline ✅ |

### 13.2 Qwen Code（QwenLM/qwen-code v0.24.4，2026-09-23 核实）

| 项 | 结论（源码确证） |
|---|---|
| fork 关系 | 基线 gemini-cli v0.8.2，**自 v0.1 起停止同步**（README 明示）；monorepo 已扩张至约 20 包（daemon/desktop-shell/channels 钉钉飞书微信等） |
| 会话落盘 | `<runtime>/projects/<sanitizeCwd(项目根)>/chats/<sessionId>.jsonl`（**纯追加消息树**，uuid/parentUuid，形态接近 Claude Code；每行自带 cwd/version/gitBranch）；sanitizeCwd = Windows 小写＋非字母数字转 `-`（不可逆，但行内 cwd 直取无需反解）；文件名即会话 uuid（32~36 位 hex-dash）；源码注释里的 `tmp/<hash>/chats/` 是**陈旧注释**（实际走 getProjectDir()，旧 tmp 路径仅 legacy 兼容读取） |
| 重定向 | `QWEN_HOME`（全局配置根：settings/oauth/skills）＋`QWEN_RUNTIME_DIR`（运行时输出基目录：tmp/chats/projects，优先级高于 settings 的 runtimeOutputDir——settings 分支本期不读，装机核实）；⚠️ 总纲旧记录「QWEN_DIR」实为项目级目录名常量 `.qwen`，**不是环境变量** |
| token 落盘 | assistant 行 `usageMetadata:{promptTokenCount, candidatesTokenCount, totalTokenCount, cachedContentTokenCount, thoughtsTokenCount}`（字段名同上游 `@google/genai`）；值按协议**归一化**：OpenAI 兼容（含 DashScope）cached=prompt_tokens_details.cached_tokens ⊆ prompt；Anthropic 协议 prompt=input+cache_read+cache_creation 三者和——**统一按 cached ⊆ prompt 拆分入库**；thoughtsTokenCount 缺失时按思考文本估算（估算值当参考） |
| 观测 sidecar | ✨ `chats/<sessionId>.runtime.json`（snake_case，schema_version=1：pid/session_id/work_dir/hostname/started_at/qwen_version）——**官方注释明言为 terminal multiplexers/IDE integrations/observability daemons 设计**；原子写、退出/崩溃不删除需自验 pid。本期不采（列装机清单：验证真实形态后可作进程级活跃信号增强） |
| hooks | settings.json `hooks` 键：22 事件 PascalCase，HookDefinition 数组同 Gemini；**有 async 字段**（true=立即返回零阻塞，并发上限 10）＋`shell:"bash"\|"powershell"`（省略时 Windows 默认可能是 cmd.exe/Git Bash 三态不确定——注入显式 powershell）；timeout 单位**秒**（≥1000 按旧毫秒语义读，默认 60）；判重身份键=name（无 name 用完整 command）；StopFailure/MessageDisplay/SessionDelete 走内置 node 监督脚本 detached 运行不受超时限制；顶层 `disableAllHooks:true` 可整体急停；运行中会话**不热加载** hooks（需重启会话） |
| hooks stdin | 基础 `{session_id, transcript_path, cwd, hook_event_name, timestamp, permission_mode, (prompt_id), (agent_id)}` 全 snake_case；StopFailure＋`error`（**枚举**：rate_limit/authentication_failed/billing_error/invalid_request/server_error/max_output_tokens/loop_detected/unknown）；PostToolUseFailure＋error 文本/工具名；PermissionRequest＋tool_name/tool_input/permission_mode；Stop＋input_tokens/context_usage 等 |
| 注入事件集（10） | SessionStart/UserPromptSubmit/PreToolUse/PostToolUse/PostToolUseFailure/Notification/PermissionRequest/Stop/StopFailure/SessionEnd——**全部已在状态机映射表内（零映射改动）**，last_failure/waiting 通道直接复用 Kimi 先例；条目 `{hooks:[{type,command,timeout:10,async:true,shell:"powershell"}]}` |
| OTel outfile | `telemetry.outfile`（env QWEN_TELEMETRY_OUTFILE）同构（file-exporters.ts 同源）；指标/日志事件名 `qwen-code.*` 前缀；默认建议路径 `.qwen/telemetry.log`；`logPrompts` 默认 true |
| 进程 | npm `@qwen-code/qwen-code`（bin=qwen→dist/index.js）＋standalone 安装 `%LOCALAPPDATA%\qwen-code\bin\qwen`——两形态命令行都含 qwen-code；node ≥22 |
| 状态可达 | working=chats mtime 启发式＋hooks 10 事件；waiting=PermissionRequest/Notification（hooks 注入后）；error=StopFailure/PostToolUseFailure（**精确枚举错误类型**，优于通知文本启发式）；idle/offline ✅（runtime.json pid 自验可增强，装机核实） |

### 13.3 待装机核实清单（装机后回写本节并跑 test_real_*）

1. Windows 实际落盘核对：`~\.gemini\tmp\<slug>\chats\session-*.jsonl`（slug 形态、
   旧 hash 目录迁移痕迹）与 `~\.qwen\projects\<sanitizeCwd>\chats\<uuid>.jsonl`
2. 真实转录样本对账：Gemini tokens.input 是否 prompt 原值（拆分口径验证，与
   `/stats` 比对）；同 id 重 append 实证与 `gm:` 幂等重采零重复；`$rewindTo`
   回滚后历史用量行残留量评估
3. Qwen：custom_title 行标题的**确切载体字段**（本期顶层 title 宽容提取）；isSidechain
   子代理行的用量归属；`turn_result` subtype 是否携带可提取的回合失败形态（若可
   则补转录内 error 信号）；runtime.json 真实形态与 pid 存活验证
4. Qwen settings 内 `runtimeOutputDir` 分支的使用率（决定是否补读）
5. Gemini 子代理会话文件（chats/<父id>/<子id>.jsonl）是否补采（用量真实存在，
   需评估会话列表污染）
6. hooks 真实触发链：两家注入后 headless 触发（对齐 CC test_real_hooks_e2e）；
   Gemini 同步 hook 的 spawn 开销实感；Qwen async 并发上限实践
7. M2-12 前提：OTel outfile 真实样本（pretty JSON 流括号配平解析验证；Gemini
   混写三类记录、Qwen 命名空间）；logPrompts 关闭引导文案
8. 两家可达状态集按 §2.7（04-EXPANSION）回写
9. （M2-12 增补）telemetry.outfile 的**路径形态**：CLI 侧是否展开 `~/`、相对路径
   以何为基准（Node fs 相对 cwd——我们对展开/原样都支持，相对路径 stat 不到即静默）
10. （M2-12 增补）两家 attributes 的 `session.id` 与转录 sessionId **同源验证**
    （决定 outfile 错误行能否精确归到转录同名会话；Qwen 侧 logApiResponse 有显式
    sessionId 参数覆盖路径，更需实证）
11. （M2-12 增补）超大 outfile 首读性能（CLI 无轮转，无限增长；如需再加尾段起点
    策略）＋`test_real_otel_gemini`/`test_real_otel_qwen` 补跑（含与 `/stats` 对账、
    token 行是否切主通道的最终裁定）
12. （M2-12 增补）`GEMINI_TELEMETRY_ENABLED`/`QWEN_TELEMETRY_ENABLED` env 变量名
    核实（Qwen 已源码确证 v0.24.4 config.ts:129；Gemini 同构推定）

### 13.4 OTel outfile 字段级调研（M2-12 开工前核实，2026-09-23）

> 核实方式：gemini-cli v0.60.0 本地克隆仓（E:\AIAgentTemp\ZCode-gemini-hooks-research\repo）
> ＋qwen-code v0.24.4 GitHub 逐文件（telemetry 六件）＋OTel 上游 opentelemetry-js
> sdk-logs 0.218.0 源码（gemini-cli 依赖版本）。§13.1/§13.2 的 outfile 行全部复核
> 属实，本节补字段级细节。

| 项 | Gemini CLI v0.60.0 | Qwen Code v0.24.4 |
|---|---|---|
| 文件写出 | `safeJsonStringify(data,2)+'\n'` 追加（file-exporters.ts），span/log/metric 三类 exporter **同文件混写**；metric temporality=CUMULATIVE、每 10s 导出 | 同构（sdk-impl.ts:316-321）；新事件才有 `qwen-code.` 前缀，api_response/api_error **无前缀** |
| api_response log | **双记录**：`toLogRecord`（event.name=`gemini_cli.api_response`，attributes 带 6 项计数＋model＋duration_ms＋prompt_id＋session.id）＋`toSemanticLogRecord`（event.name=`gen_ai.client.inference.operation.details`，仅 gen_ai.usage.input/output 2 项）——**必须按 event.name 过滤，两条都取必双计**（loggers.ts:319-320） | **单记录**：attributes = getCommonAttributes ＋ **事件顶层展开**（token 5 项无 tool 项）＋显式 sessionId 可覆盖 session.id（loggers.ts:611-656） |
| token 字段（attributes 顶层同名） | input/output/cached_content/thoughts/tool_token_count＋total | input/output/cached_content/thoughts/total（无 tool）；多 response_id/ttft_ms/subagent_name |
| api_error | attributes：`error.message`/`error`/`error.type`（可选）/model/status_code/duration_ms/session.id | attributes 顶层展开：`error_message`/`error_type`（可选）/model/response_id/session.id |
| session 关联 | getCommonAttributes 带 **`session.id`**（telemetryAttributes.ts:15）＝config.getSessionId()，与转录 metadata.sessionId 同源（装机核实） | getCommonAttributes 仅 `{session.id}`（loggers.ts:156） |
| 记录 JSON 形状 | **只有 resource/instrumentationScope/attributes 三个可枚举键**——OTel sdk-logs 0.218.0 LogRecordImpl 的 timestamp/body/severity 全是私有字段不落盘；时间只能取 attributes.`event.timestamp`（两家自带 ISO 毫秒） | 同（sdk-logs 同代） |
| 启用前提 | `telemetry.enabled=true` **且** outfile 有值（sdk.ts:169 enabled=false 整个 SDK 不初始化）；发现链 argv ?? `GEMINI_TELEMETRY_OUTFILE` ?? settings.telemetry.outfile（config.ts:106-109） | 同构（sdk-impl.ts + config.ts:127-130/195-196）；env 名 `QWEN_TELEMETRY_ENABLED`/`QWEN_TELEMETRY_OUTFILE` 已确证 |
| 隐私 | logPrompts=true（默认）时 attributes 携带 response_text/request_text/gen_ai.*.messages **全文**——解析白名单取数（总纲 §2.8.2） | 同（response_text 顶层展开） |

实施要点（对应 collector/otel.rs）：解析只依赖 attributes（新旧 SDK 兼容最稳面）；
StreamDeserializer 按值配平＋精确字节游标（**不可复用引擎 64KB 回退的
IncrementalFileReader**——重读会重复产出）；截断半条停值起点待补读；UTF-8 尾部
多字节残缺只解析合法前缀（lossy 替换会使字节偏移失真）；中段坏数据从**失败点**向后
跳行防卡死（从 last_good 跳只越过值间空白会空转——单测实锤过）；token 拆分与转录
同口径（input−cached，cached ⊆ prompt；tool/total 不入账）。
通道裁定见 03-TASKS M2-12：token 行暂不入库防双计，api_error 行入库走 recent_error。

---

## 14. 各 Agent 数据面勘察（P3，2026-09-24，M2-13 OpenClaw 开工前源码级核实）

> 核实方式：openclaw/openclaw main 分支 GitHub 逐文件源码（state/config/sessions
> 四域 schema、transcript 持久化器、状态目录解析器、用量归一化器）；所有者本机
> 未装机，全部结论以源码为准，装机实测留 §14.3 清单。

### 14.1 状态目录与多 agent 多库

| 项 | 结论（源码出处） |
|---|---|
| 状态根解析 | `resolveStateDir`（config/state-dir.ts）：`OPENCLAW_STATE_DIR` env 非空即重定向（home 相对路径可用）> `~/.openclaw`（新版默认）> `~/.clawdbot`（更名前旧目录，**仅当新版目录不存在时回退**；两者并存时新版优先） |
| 多 profile | `resolveProfileStateDir`（cli/profile-utils.ts:31-42）：命名 profile 的状态根是 `~/.openclaw-<小写名>` 后缀目录——枚举 `~/.openclaw*` 一并覆盖 |
| 每 agent 一库 | `resolveOpenClawAgentSqlitePath`（state/openclaw-agent-db.paths.ts:45-67）：`<root>/agents/<agentId>/agent/openclaw-agent.sqlite`——**多 agent 多库**，与 OpenCode 单库不同，须目录枚举；自库会话 id 必须带 agentId 消歧（各库 session_key 同名，如 'main'） |
| incognito 哨兵 | `incognito-openclaw-agent.sqlite` 同目录保留名（INCOGNITO_AGENT_SQLITE_BASENAME）：内存态进程哨兵，不匹配 `openclaw-agent.sqlite` 文件名自然排除 |
| agentId 归一 | `normalizeAgentId`（routing/session-key.ts）——大小写归一等；适配器按目录名原样采即可 |
| schema 版本 | `schema_meta` 表（schema_version/agent_id/app_version）——版本漂移观察点 |

### 14.2 会话三层结构与用量事件面

| 层 | 表/关键列 | 采集语义 |
|---|---|---|
| 逻辑会话 | `session_nodes`：session_key PK（跨代稳定）、current_session_id、entry_json（canonical 记录）、status（running/done/failed/killed/timeout，**现成状态信号**）、display_name/label（标题链）、created_at/updated_at/last_activity_at（毫秒；last_activity_at=最近完成回合）、project_id | scan 主表；标题 display_name>label；last_usage=last_activity_at 缺失回退 updated_at |
| 转录代 | `session_windows`：session_id PK（reset/rollover/fork/rewind/switch/recovery/compaction 时**轮换**）、session_key FK、previous_session_id、model/model_provider 列、started_at/ended_at、status | 用量窗口选择面（updated_at 水位）；模型列可选（聊天通道场景多缺，回退用量流水回填） |
| 事件流水 | `transcript_events`：(session_id,seq) 主键、event_json 明文 **或** event_zstd 压缩（互斥 CHECK）、event_utf8_bytes（解压后字节数校验）、created_at | collect 增量面；session_id 跨代轮换 → 按窗口 updated_at 选窗 + per-window seq 水位 |

**zstd 压缩策略**（config/sessions/transcript-payload.ts）：事件明文 ≥1024 字节、压缩后节省 ≥max(64B,10%)、不含 `\u`/NUL、非 session header → 转 zstd（level 1）。**assistant 消息带正文通常超 1KB → 压缩是常态**，Rust 侧 `zstd` crate 解压是硬前提；event_utf8_bytes 用于解压长度校验（防坏解压污染用量）。

**用量事件形状**（agents/command/transcript-persistence.ts `persistTextTurnTranscript`）——assistant 消息事件：

```json
{"type":"message","message":{"role":"assistant",
  "assistantIdempotencyKey":"…",          // 可选，官方幂等键
  "content":[{"type":"text","text":"…"}],
  "api":"anthropic-messages","provider":"anthropic","model":"claude-opus-4-6",
  "usage":{"input":120,"output":60,"cacheRead":30,"cacheWrite":10,
           "total":220,"cost":{…},"contextUsage":{…}},
  "stopReason":"stop|error|aborted","timestamp":1727…}}
```

- usage 四桶 camelCase，`resolveTranscriptUsage` 落盘前已归一（**input 不含缓存**，直取免拆分）；total 是上下文快照语义（SessionEntry.totalTokens 注释明示 excludes output），不采；
- **回合式一次性落盘**（finalText 就绪才 append，无 CC 式流式中间快照）→ usage 即每回合增量，无双计面；
- usage 缺失时 CLI 后端回退全零占位（CLITranscriptUnavailableUsage）——此类回合无统计价值，跳过；stopReason=error 且无 usage → 只作错误行入库（error_type 弱信号「回合失败」，精确错误载体装机核实）；stopReason=aborted 为用户打断，不计错误（对齐 ZCode cancelled_by_user 裁定）；
- 事件类型 union（config/sessions/session-entry-codec.ts）：message/thinking_level_change/model_change/compaction/reset/branch_summary/custom/custom_message/label/session_info——只消费 `type=="message"`；header=`{"type":"session","id":…}` 跳过；
- 进程：openclaw gateway 常驻 node 进程（端口 18789 为 docs 通行值，装机核实）；无 CC 式 hooks 文件协议（Gateway HTTP 端点属 LocalHttp 可选增强档，暂不做）。

### 14.3 待装机核实清单（装机后回写本节并跑 test_real_openclaw）

1. 落盘核对：`<root>/agents/*/agent/openclaw-agent.sqlite` 真实路径、库文件/-wal 布局（node:sqlite DatabaseSync 的 journal 模式）
2. 用量对账：`openclaw status`/sessions 输出（SessionStatus 的 inputTokens/outputTokens/cacheRead/cacheWrite 四项）与自库分项比对
3. `entry_json` 内 cwd/workspace 载体字段（当前 project_dir 采集为空，前端回退「未知项目」）
4. stopReason=error 的精确错误载体（error 文本/error code 是否落 message）
5. aborted 回合的 usage 语义（打断时是否有部分用量入账）
6. 窗口轮换（compaction/reset）后 usage 跨代归并：usageFamilyKey/usageFamilySessionIds（SessionEntry）是否需要随行采纳
7. gateway 进程名与端口（18789）实核；`OPENCLAW_STATE_DIR` 语义复核
8. last_activity_at vs last_interaction_at 语义差（入口路径 last_interaction_at 的写入时机）

---

## 15. 各 Agent 数据面勘察（P3，2026-09-24，M2-14 Hermes 开工前源码级核实）

> 核实方式：NousResearch/hermes-agent main 分支本地克隆逐文件源码（hermes_state_*
> 20 余模块：schema/usage/messages/rewind/ids＋agent/turn_usage＋hermes_cli/profiles/
> plugins＋hermes_constants）；所有者本机未装机，全部结论以源码为准，装机实测留
> §15.3 清单。总纲预想「state.db 核心持久层」属实，但**预想表结构已大幅修正**。

### 15.1 状态根、多 profile 与库文件

| 项 | 结论（源码出处） |
|---|---|
| 状态根解析 | `get_hermes_home`（hermes_constants.py:112）：`HERMES_HOME` env（expanduser+expandvars）> 平台默认；**Windows=`%LOCALAPPDATA%\hermes`**（LOCALAPPDATA 缺失回退 `~\AppData\Local\hermes`，_get_platform_default_hermes_home:53），README 安装节同口径确证；非 Windows=`~/.hermes` |
| 多 profile | 命名 profile 的 home=`<默认根>/profiles/<名>/`（hermes_cli/profiles.py:174——**profiles 根锚定平台默认根而非当前 HERMES_HOME**，_get_default_hermes_root 当 HERMES_HOME 形如 `<root>/profiles/<名>` 时反解 root）；适配器枚举 默认根＋默认根/profiles/*，tag=default/<名> |
| 库文件 | `<home>/state.db`（hermes_state.py:168 DEFAULT_DB_PATH），**WAL 模式**（模块 docstring：「WAL mode (concurrent readers + one writer)」）；守护模块（hermes_state_guard/lockguard/errors/dbfile 等）是 Hermes 自进程的锁/损坏修复/文件句柄稽核，对外部只读读者无额外要求 |
| schema 版本 | `schema_version` 表，当前 **SCHEMA_VERSION=30**（hermes_state_common.py:239）；迁移内建（列对账 _reconcile_columns），只读面按列名白名单容忍漂移 |

### 15.2 会话数据面与用量记账（与总纲预想的四点差异）

| 项 | 结论（源码确证） |
|---|---|
| ⚠️ 差异①：无逐调用流水表 | **预想「SqliteTail 水位采集 usage 行」不成立**——库内没有逐 API 调用行；messages 表逐条消息仅 `token_count` 单值整数（无四桶分项）。唯一四桶数据面＝**累计快照**（sessions 主行＋session_model_usage 行）→ 采集只能走「累计快照重采＋保留最大快照幂等」路线（store 现有 upsert 语义天然适配） |
| ⚠️ 差异②：记账双通路 | `update_token_counts` 是**所有 API 调用记账的唯一咽喉**（hermes_state_usage.py 注释明示：CLI/gateway/cron/delegated 全走此）；CLI 面=增量 delta（`absolute=False`，api_call_count=1，sessions 累加＋session_model_usage UPSERT 累加）；gateway 面=**absolute 绝对值覆盖 sessions 主行**（远程面持有累计总数；「absolute cumulative updates cannot be split back into routes」——session_model_usage 不被 absolute 写，保持增量累加）；落盘经后台合并 writer 线程（queue_token_counts，秒级延迟，atexit 兜底 flush）——最终一致，快轮读到的是略旧累计，无害 |
| ⚠️ 差异③：辅助调用自带维度 | `session_model_usage.task` 列区分消耗类型：`''`=主循环；非空=辅助调用（vision/compression/title_generation/background_review…，record_auxiliary_usage 只写此表不写 sessions 主行，#23270）——**正是 CC cost-state 后台用量的对应物**，直接映射 is_background=1（token 计入、次数不计，M1-12 裁定口径） |
| ⚠️ 差异④：无错误载体 | 库内**无逐调用错误列**（messages.finish_reason 是生成终止原因非错误；cost_status 是计费状态）——总纲矩阵「各家库内错误字段装机核实」已核实完毕：**不存在**；`api_request_error` 钩子事件存在但仅 hooks 面可及（本期不接 hooks）→ 错误状态纯启发式 |
| 会话表 sessions | id=`YYYYMMDD_HHMMSS_<hex>`（hermes_state_ids.py，交互面 6 hex/gateway 8/导入 12）；source（cli/tui/telegram/discord/cron/subagent/kanban/tool/oneshot…）；title（AI 生成，title_source 排名去重）＋display_name（聊天面会话名）双列；cwd/git_branch/git_repo_root；model/billing_provider/base_url/mode；**五桶累计列**（input/output/cache_read/cache_write/reasoning_tokens）；estimated/actual_cost_usd；started_at/ended_at（REAL Unix 秒）/end_reason（reset 类 6 种＋可恢复类 4 种＋compression/new_session）；**last_activity_at 单调推进**（UPDATE 带 `< ?` 守卫，hermes_state_sessions.py:606）；parent_session_id（子代理/分支/压缩子代）；archived/pinned/hidden |
| 累计表 session_model_usage | 主键 (session_id, model, billing_provider, billing_base_url, billing_mode, task)；五桶＋api_call_count＋双 cost＋first_seen/last_seen（REAL 秒，每次累加刷新） |
| 采集口径裁定 | source_id=`hm:{tag}:{session_id}:{model}:{task}:{provider}:{base_url}:{mode}`（六元组主键全拼防碰撞）；ts=last_seen×1000；**已知口径差**：历史会话的 token 全记到 last_seen 末日（无逐调用时间戳可用），报表按天分组时历史压缩到末日——装机对账评估 |
| 单调性 | 累计快照「保留最大」策略安全：hermes_state_rewind.py **不清零 token 列**；增量路径只加不减；gateway absolute 只覆盖 sessions 主行（session_model_usage 不受影响）——理论漂移点＝absolute 主行与增量表之差（装机对账项） |
| 状态信号 | 无现成状态列；working=last_activity_at 时效启发式（快轮 state.db/-wal mtime）；ended_at/end_reason 无 SessionInfo 载体（时效窗自然覆盖）；gateway_heartbeats 表（backend_id/pid/last_heartbeat，#94895）可作进程级活性增强，装机核实 |
| hooks | **有 shell hooks 体系**（纠正总纲「待核实」）：`<home>/config.yaml` 顶层 `hooks:` 键（YAML！非 JSON/TOML），事件 pre_tool_call/post_tool_call/pre_llm_call/post_llm_call/pre/post_api_request/api_request_error/on_session_start/end/finalize/reset/subagent_stop 等（hermes_cli/plugins.py VALID_HOOKS 更全）；条目 `{matcher?, command, timeout}`，shlex.split 子进程＋JSON stdin＋stdout 可阻断/注入；**consent 机制**：每 (event, command) 对 TTY 首次确认、持久化 shell-hooks-allowlist.json，非交互面须 `--accept-hooks`/`HERMES_ACCEPT_HOOKS=1`/`hooks_auto_accept` ——注入成本=YAML 保格式注入器（第三种形态）＋consent 双文件，且 SQLite 通道已覆盖用量与启发式状态 → **本期不接，列装机后增强档**（照 OpenCode SSE 增强档先例） |
| 进程 | Python 应用：`hermes` 启动脚本（仓库根）＋常驻 gateway 进程（单进程多平台：Telegram/Discord/Slack/WhatsApp/Signal）＋TUI；进程名 python 无特征，命令行含 hermes（cmd_keywords 装机核实） |
| foreign_sessions | Hermes 侧「导入 CC/Codex 会话」的功能（hermes_cli/foreign_sessions.py，只读别人文件）——方向相反（它读别人，我们读它），与适配器无关 |
| 官方统计口径 | usage_totals（dashboard 侧栏）：`parent_session_id IS NULL AND message_count>=1 AND NOT archived`——官方排除子会话/空会话/归档；我们 scan 全收（90 天窗），口径差装机对账 |

### 15.3 待装机核实清单（装机后回写本节并跑 test_real_hermes）

1. 落盘核对：`%LOCALAPPDATA%\hermes\state.db`（及 profiles/<名>/state.db）真实存在性、
   WAL sidecar 布局；HERMES_HOME 实际使用形态
2. 用量对账：`hermes insights`/dashboard 的会话 token 与自库 session_model_usage 快照比对；
   gateway absolute 覆盖后 sessions 主行与增量表 SUM 的漂移量
3. 报表按天口径：last_seen 记账的「历史压缩到末日」偏差实测（决定是否按 first_seen 分摊）
4. 进程名与命令行形态（gateway/TUI 常驻 python 的 cmdline 特征）；gateway_heartbeats
   真实行形态（backend_id/pid/last_heartbeat）与活性窗口
5. 多 profile session id 跨库碰撞观察（id 含 uuid，理论概率可忽略）；hidden/archived
   会话的展示口径
6. messages.token_count 单值语义（若四桶快照对账顺利则维持不采）
7. hooks 增强档决策：api_request_error/on_session_* 注入的实测价值（YAML 注入器＋
   consent allowlist 成本 vs 状态精度增益）
8. 可达状态集按 §2.7（04-EXPANSION）回写

---

## 调研日志

| 日期 | 进展 |
|---|---|
| 2026-09-16 | §1 ZCode 勘察完成（决定性）；§2/§3 基于早期调研归档 |
| 2026-09-16 | §4 轮子盘点完成（全🟢）；§5 确认 Tauri 无现成岛→自建；§6 hooks 改实测策略；§7 GLM 响应格式源码级确认——**阶段 2 调研收官** |
| 2026-09-17 | §9 报表页选型完成（echarts@6.1.0 直用+按需引入+lazy 分割），M1-1 开工 |
| 2026-09-17 | §10 Codex CLI 源码级勘察完成（sessions/*.jsonl + TokenCount 事件），适配器待实测后开发，M1-3 第一步收官 |
| 2026-09-23 | §11 新增（P1 勘察节）：Codex/Kimi Code 两家源码级核实完成（serde 属性级）；Kimi 记录纠正为新版 kimi-code；M2-6/M2-7 按此实施，真实对账留待装机 |
| 2026-09-23 | §12 新增（P2 勘察节）：OpenCode/MiMo Code 源码级核实（Schema 字段级+drizzle 表列级）；**重大纠正——OpenCode 主存储已迁 SQLite**（总纲 JSON 记录过时），通道优先级反转为 SQLite 主+SSE 增强；M2-10a 按此实施 |
| 2026-09-23 | §13 新增（P2 勘察节）：Gemini CLI/Qwen Code 两轮源码级核实（数据面+hooks 字段级）；**三处纠正总纲预想**——同族参数化不成立（fork 已分叉，独立双适配器）、Gemini 无 hooks 不成立（11 事件）、chats 已 JSONL 化（同 id 重 append）；token 口径裁定 cached ⊆ prompt；M2-11 按此实施（主通道+hooks 注入一并，所有者拍板扩大范围） |
| 2026-09-23 | §13.4 新增（M2-12 开工前 outfile 字段级调研）：**Gemini 双记录须按 event.name 过滤防双计**、Qwen 单记录顶层展开、outfile JSON 仅 attributes 可枚举（时间取 event.timestamp）、启用需 enabled+outfile 双前提；OTLP receiver 仍留 backlog；M2-12 按此实施（token 行暂不入库防双计，所有者拍板） |
| 2026-09-24 | §14 新增（P3 勘察节）：OpenClaw 源码级核实（schema 建表 SQL+转录持久化器+状态根解析器）；**三处总纲外新知**——①多 agent 多库（每 agentId 一库，session_key 跨库同名须带 agentId 消歧）、②事件 ≥1KB 转 zstd 是常态（Rust 侧必须解压+utf8_bytes 校验）、③回合式一次性落盘无双计面（usage 即每回合增量）；input 已归一免拆分；aborted 不计错误对齐 ZCode；M2-13 按此实施（合成样本 7 单测，装机对账留 §14.3） |
| 2026-09-24 | §15 新增（P3 勘察节）：Hermes 源码级核实（hermes_state_* 全模块+turn_usage+profiles/plugins）；**四处纠正总纲预想**——①无逐调用流水表（采集改累计快照重采+保留最大幂等）、②记账双通路（CLI 增量/gateway absolute 只覆盖主行）、③task 列即后台辅助调用维度（对齐 CC cost-state is_background）、④库内无错误载体（hooks 面 api_request_error 本期不接，YAML+consent 成本）；Windows 根=`%LOCALAPPDATA%\hermes`+profiles/<名> 枚举；M2-14 按此实施 |
