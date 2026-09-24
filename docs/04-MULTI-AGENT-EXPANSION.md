# AgentTrackerIsland 实时性升级与多 Agent 扩展总体方案（04-EXPANSION）

> 状态：**v1.1 定稿，2026-09-22**（v1.0 全网调研 + 所有者评审；v1.1 二次自查细化——补状态可达性矩阵、隐私细则、降级矩阵、接口草案与验收量化，见 §2.7~§2.9 与附录 A）
> 上游：[01-RESEARCH](01-RESEARCH.md)（单 Agent 数据面勘察）、[02-DESIGN](02-DESIGN.md)（现架构）、宪法（AGENTS.md）
> 定位：M2 阶段总纲——实时性改造 + 14 个主流 Agent 状态监控接入的架构设计与分期计划。
> 本文与 02-DESIGN 冲突时以本文为准（定位升级：单 Agent 观测 → 多 Agent 平台）；
> 任务开工时在 [03-TASKS](03-TASKS.md) 以 M2 系列编号登记。

---

## 0. 背景与问题定义

### 0.1 两个待解问题

| # | 问题 | 根因（2026-09-22 代码核查结论） |
|---|------|------|
| 1 | **实时性弱**：用户开始与 Agent 对话后，岛/面板最长 10s 无反应 | 全链路只有 `spawn_aggregator` 固定 `sleep(10s)` 一个节拍（src-tauri/src/lib.rs），hook 事件消费、采集、状态计算、广播全部搭这一班车；前端是纯被动接收方，无问题 |
| 2 | **状态保真缺陷**：ZCode 长回答生成期间（>90s 无调用完成落库）状态从「工作中」掉回「空闲」 | ZCode 的 `model_usage` 行是调用完成后才写库；活动启发式窗口 `ACTIVITY_FRESH_MS = 90s` 过期即降级（src-tauri/src/state/mod.rs） |

### 0.2 扩展目标（所有者拍板：2026-09-22 首批 12 家，同日追加至 14 家）

接入以下 14 个 Agent 的状态监控（清单即需求，非猜测）：
**ZCode、Claude Code、Codex、Gemini、Kimi Code、OpenClaw、Hermes、WorkBuddy、OpenCode、Cursor、Windsurf、Copilot、MiMo Code（小米）、Qwen Code（千问）**。

判断前提：全球 Agent 层出不穷，无法预判谁会火，因此架构必须保证「接新 Agent = 低成本、有 SOP、不动核心」。

### 0.3 非目标（宪法§四不变）

- ❌ 模型代理 / 请求转发 / 任何流量中间人（红线①）
- ❌ 在工具内直接操作 Agent（遥控器）
- ❌ 为覆盖某 Agent 而引入常驻后台服务、系统级注入等重侵入手段
- ❌ 一步到位接满 14 家——按 §3 分期逐家交付，每家独立可用

---

## 1. 调研结论（2026-09-22 全网调研，agent-reach：Exa/GitHub/官方文档）

### 1.1 行业四条实时数据路径

| 路径 | 代表 | 对本项目适用性 |
|------|------|--------------|
| 📁 文件监听/转录 tail | claude-team-dashboard（WS+文件 watcher）、onikan27/claude-code-monitor（310★）、各 menubar 应用 | ✅ 主流做法，成本最低 |
| 🪝 Hooks 事件镜像 | Happy Coder（wrapper+hooks 上云）、Arize tracing | ✅ 我们已有此机制，可跨 Agent 复用 |
| 📡 OTel 官方遥测 | Anthropic 官方监控指南、Gemini CLI 原生 OTel、Codex（OTel events） | ⚠️ 企业级标准；Agent 主动推送不违反红线①，作 opt-in 增强档 |
| 📦 包装器/SDK 代理 | Happy（包装 CLI）、Omnara（SDK 内嵌） | ❌ 侵入 Agent 本体，违反红线①②，排除 |

补充信号源：Claude Code 的 **statusline 通道**（对话更新时以 stdin JSON 回调状态栏命令）是零安装的实时补充信号（ccstatusline 类工具依赖它）。

### 1.2 十四 Agent 可观测面矩阵（核实日期 2026-09-22）

| 档位 | Agent | 会话/转录文件（可增量 tail） | 生命周期事件 | 用量信号 | 接入通道 |
|------|-------|------------------------------|-------------|---------|---------|
| 🅰 已接 | Claude Code | `~\.claude\projects\**\*.jsonl` | hooks 7 事件 + statusline + OTel | 转录 usage + cost-state | 已运行 |
| 🅰 已接 | ZCode | `~\.zcode\cli\{db\db.sqlite, log\zcode-日期.jsonl, rollout\model-io-sess_*.jsonl}` | 无（自有工具，可加 hooks） | sqlite model_usage | 已运行（P0 补日志活动信号） |
| 🅱 富 | **Codex** | `~\.codex\sessions\YYYY\MM\DD\rollout-{ISO时间戳}-{UUID}.jsonl`（CODEX_HOME 可重定向；**2026-09-23 源码核实**，详 01-RESEARCH §11.1） | **hooks 12 事件与 CC 几乎同名**（多 PermissionRequest/PreCompact/PostCompact/SubagentStart/SubagentStop/Interrupt）；载体=config.toml `[hooks]` 段（`[[hooks.<Event>]]`→MatcherGroup→handler，deny_unknown_fields）+ hooks.json 双载体（源码核实）；legacy notify（agent-turn-complete，JSON 走末位 argv）；OTel | `token_usage_record` 行 `response_id` 幂等（snake_case TokenUsage；⚠️ cached 是 input 子集，已按互斥口径拆分）；`event_msg→token_count` 退路 | FileTail + HookBridge ✅ M2-6 |
| 🅱 富 | **Kimi Code** | `~\.kimi-code\sessions\<wd_key>\<sid>\{state.json, agents\<agentId>\wire.jsonl}`（**2026-09-23 源码核实，纠正本表旧版记录**：主线是 TS 的 kimi-code，旧 Python kimi-cli 已归档；KIMI_CODE_HOME 重定向，旧 KIMI_SHARE_DIR 已不生效；详 01-RESEARCH §11.2） | **hooks 20 事件**（CC 同名超集 + TurnStarted/PermissionRequest/PostToolUseFailure/StopFailure/Interrupt/SessionHeartbeat 等），stdin JSON，TOML 顶层 `[[hooks]]`（仅 event/matcher/command/timeout 四字段） | wire.jsonl `usage.record` 行（camelCase 四字段 inputOther/output/inputCacheRead/inputCacheCreation；行无官方 id → 内容指纹幂等；⚠️ usageScope 语义装机核实） | FileTail + HookBridge ✅ M2-7 |
| 🅱 富 | **OpenCode** | **主存储已迁 SQLite**（2026-09-23 源码核实，**纠正本表旧版记录**：`~\.local\share\opencode\opencode.db`，Windows 无 XDG 特判；drizzle message 表 data JSON 列带 tokens 五项+cost，session 表另有聚合列；JSON storage 仅剩回滚/导入边缘用途；详 01-RESEARCH §12.1） | 无 CC 式 hooks（plugin 体系不适用观测）；`opencode serve` 的 `/event` SSE（`session.next.step.ended` 带 usage，127.0.0.1 默认+随机端口→**端口发现是装机核实点**） | message 行 `tokens{input,output,reasoning,cache.read,cache.write}`+cost，`msg_` id 幂等；cache 为独立分项语义 | SqliteTail 主通道 ✅ M2-10a + LocalHttp SSE 增强档（M2-10b 装机后） |
| 🅱 富 | **MiMo Code**（小米） | **OpenCode fork 确证**（基线=SQLite 化后、core 拆分前）；`~\.local\share\mimocode\mimocode.db`（**2026-09-23 源码核实，纠正旧记录**：重定向是 `MIMOCODE_HOME`，非 `.mimocode`/`MIMOCODE_CONFIG_DIR`）；session 表无聚合列，用量逐行取 message | 沿用 OpenCode 能力（`mimo serve`/SSE 同款，装机核实）；另有 plugin 体系 | message data JSON 平铺 `{modelID,providerID,cost,tokens{...}}`，`msg_` id 幂等 | **与 OpenCode 同一适配器参数化复用** ✅ M2-10a（差异仅 agent id/数据根/db 文件名）+ SSE 增强档 M2-10b |
| 🅲 中 | **Gemini CLI** | `~\.gemini\tmp\<项目slug>\chats\session-*.jsonl`（**2026-09-23 源码核实，纠正本表旧版记录**：已 JSONL 化＋同 id 消息重 append＋$set/$rewindTo 控制行；目录名由 hash 改为 projects.json 注册表 slug；详 01-RESEARCH §13.1） | **有 hooks 11 事件 CC 式**（纠正「无 hooks」旧记录；settings.json `hooks` 键，无 async 同步执行）；OTel outfile 确证（pretty JSON 流需括号配平） | tokens 落盘 input=prompt 原值，**官方口径 input = prompt − cached**；error 消息行 | FileTail ✅ M2-11 + HookBridge ✅ M2-11（7 事件）+ **OtelSink ✅ M2-12**（outfile 骨架：api_error 行入库，token 行装机对账后定，详 01-RESEARCH §13.4） |
| 🅲 中 | **Qwen Code**（千问） | `<runtime>\projects\<sanitizeCwd>\chats\<uuid>.jsonl` 纯追加消息树（**2026-09-23 源码核实，纠正「同 Gemini 同构」旧记录**：fork 基线 v0.8.2 自 v0.1 起停止同步，落盘已大幅分叉→独立适配器；`QWEN_HOME`+`QWEN_RUNTIME_DIR` 双重定向；官方观测 sidecar `chats/<sessionId>.runtime.json` 装机核实；详 01-RESEARCH §13.2） | **有 hooks 22 事件**（几乎全 CC 同名；async/shell 字段，timeout 秒） | usageMetadata 归一化后 cached ⊆ prompt 统一拆分；StopFailure.error 精确枚举 | FileTail ✅ M2-11 + HookBridge ✅ M2-11（10 事件）+ **OtelSink ✅ M2-12**（同上，单记录顶层展开形态，详 01-RESEARCH §13.4） |
| 🅲 中 | **OpenClaw** | `~\.openclaw\agents\<agentId>\agent\openclaw-agent.sqlite`（**2026-09-24 源码核实，详 01-RESEARCH §14**：多 agent 多库须目录枚举；会话三层 session_nodes→session_windows→transcript_events，事件 ≥1KB 转 zstd 是常态须解压；usage 四桶 camelCase 落盘前已按「input 不含缓存」归一；状态根 OPENCLAW_STATE_DIR>~\.openclaw>~\.openclaw-<profile>>~\.clawdbot） | 无 CC 式 hooks（Gateway 有 HTTP 端点，LocalHttp 增强档暂不做） | 事件行 usage（回合式一次性落盘无双计面）；session_nodes.status 现成状态信号 | SqliteTail ✅ M2-13 |
| 🅲 中 | **Hermes**（Nous Research） | `<home>\state.db`（**2026-09-24 源码核实，详 01-RESEARCH §15**：Windows=%LOCALAPPDATA%\hermes＋HERMES_HOME 重定向＋profiles/<名> 枚举；**库内无逐调用流水表**——唯一四桶数据面是 session_model_usage 累计快照，走重采＋保留最大幂等；task 列=后台辅助调用维度映射 is_background；schema v30，WAL） | **有 shell hooks**（YAML config.yaml `hooks:` 键＋consent allowlist——注入成本高，SQLite 通道已覆盖，列装机后增强档） | 累计快照重采（无逐调用行无双计面）；last_activity_at 单调启发式；**库内无错误载体** | SqliteTail ✅ M2-14 |
| 🅲 中 | **Copilot CLI** | `~\.copilot\session-state\` 会话文件集 + 本地 SQLite session store（官方文档，`/chronicle` 的数据源，含成本数据） | 无公开 hooks | store 内 | FileTail + SqliteTail 双选 |
| 🅳 穷 | **Cursor** | `state.vscdb`（SQLite，`~\.cursor`）——**社区逆向**（tokenuse、deja-vu registry、agent-tracker 等先例验证可行） | 无 | db 内 | SqliteTail（实验性） |
| 🅳 穷 | **Windsurf** | Cascade 轨迹本地存储——**社区逆向脚本** | 无 | 逆向 | SqliteTail（实验性） |
| ❓ 待核实 | **WorkBuddy**（腾讯 CodeBuddy 系） | 文档站未公开落盘细节，疑似 CC 同源生态 | 疑似有 hooks | 待核实 | P4 首任务装机勘察定档 |

> 维护约定：每接入一家，回写本表该行（含核实日期与来源链接）；格式漂移导致的适配器修改同样回写。

### 1.3 四个行业收敛信号（扩展性判断依据）

1. **CC 风格 hooks 正在成为事实标准**：Codex 源码含 `hooks_cla.rs`（从 CC settings.json 迁移 hooks 的官方代码），事件名高度同名；Kimi hooks 也是 CC 同名超集。→ 我们的 hook-bridge 模式**写一次可服务多家**。
2. **家家有本地会话文件/库**：转录 tail（JSONL 或 SQLite）是最大公约数，字节偏移增量读机制可完全复用。
3. **OTel 是官方钦定企业路线**：Gemini 原生支持且可 outfile 落文件（免端口）、Codex 跟进。→ 作 opt-in 通道而非主通道。
4. **fork 家族化是行业常态**（2026-09-22 追加两家时确认）：Qwen Code 是 Gemini CLI 官方 fork（目录同构）、MiMo Code 基于 OpenCode 内核——**一个内核族共用一套适配器参数**，目标清单越长，这类复用越值钱。适配器设计必须支持「同族参数化」（根路径/文件名差异配置化），而不是一家一份复制粘贴。

---

## 2. 总体架构设计

### 2.1 设计原则

1. **`AgentAdapter` 仍是唯一的 Agent 接缝**：新增 Agent 只写适配器（声明 + 解析），不改核心（宪法§三：禁止 if-else 堆砌）。
2. **引擎只实现机制，适配器只提供策略**：tail/水位/事件消费是通用机制，收拢为引擎；每家的路径 glob、行解析、事件映射是私有知识，留在适配器。
3. **状态机不动**：`compute_state` 的优先级（error > hooks > activity > process）与看门狗逻辑保持不变，各引擎只负责把 `SessionSignals` 喂得更密、更快。
4. **薄抽象（修正后的 YAGNI）**：引擎数量以本期 14 家的真实形态为界（4+2），不为想象中的形态预建框架；共性第三次重复出现时才再抽象。
5. **持久层零迁移**：`usage_records/sessions/status_events/quota_snapshots` 均已带 `agent` 维度与幂等键，多 Agent 接入不破坏旧数据（宪法§三）。

### 2.2 目标架构图

```
┌──────────────────────── WebView(React) ────────────────────────┐
│ 岛胶囊/贴边标签 │ 展开面板 │ 会话中心 │ 报表 │ 设置（多 Agent 列表）│
└───────────────────┬────────────────────────────────────────────┘
                    │ island-snapshot 广播（不变）
┌───────────────────┴──────────── Rust 核心 ─────────────────────┐
│  调度器 Scheduler：事件唤醒(recv_timeout) + 自适应快慢轮询       │
│  ┌────────────── 通用采集引擎（机制层，与 Agent 无关）────────┐ │
│  │ FileTailEngine  glob 注册→mtime/字节偏移增量读→行回调      │ │
│  │ SqliteTailEngine 只读连接+水位查询+库文件 mtime/WAL 监视    │ │
│  │ HookBridgeEngine 事件文件增量消费+事件名映射表              │ │
│  │ [可选] OtelSink(outfile 轮询/OTLP receiver)                │ │
│  │ [可选] LocalHttp(SSE 订阅)                                 │ │
│  └─────────────────────────┬───────────────────────────────┘ │
│  ┌─────────────────────────┴── AgentAdapter（策略层，每家一份）─┐ │
│  │ claude_code │ zcode │ codex │ kimi │ gemini │ opencode │ …  │ │
│  │ 声明：文件 glob/库路径/事件映射/进程匹配；解析：行→UsageRow   │ │
│  └────────────────────────────────────────────────────────────┘ │
│  状态机 compute_state（不变）│ store(SQLite 自库，schema 不变)    │
└────────────────────────────────────────────────────────────────┘
```

### 2.3 引擎规格

#### 2.3.1 FileTailEngine（现 CC 采集器机制泛化）

- 输入（由适配器声明）：`Vec<FileSource>`＝`{ glob 模式, 行解析回调, 活动信号权重 }`；运行时展开为受监视文件集。
- 机制：目录枚举（沿用 90 天 cutoff 与 100 文件截断策略）→ per-file mtime 过滤 → per-file 字节偏移增量读（回退 64KB 兜乱序）→ 逐行回调适配器解析 → 产出 `CollectOutput`（结构不变）。
- **glob 必须支持日期分区与递归**：Codex 的 `sessions\YYYY\MM\DD\` 每天产生新子目录、Kimi 是三层哈希目录。引擎枚举规则：`**` 递归 + **目录 mtime 剪枝**（目录 mtime 未变则跳过其内部枚举）——保证「新日期目录自动纳入」且每轮枚举成本可控。
- **快轮信号两种形态（HotSignal）**：适配器同时声明「回合起点信号」供调度器快轮——
  - `File(path)`：单文件 stat，如 CC hook 事件文件、ZCode 当日日志（注意日志按日轮转，路径按当日日期现算）；
  - `DirScan{ glob, depth, max_files }`：目录枚举取 max(mtime)，用于**未装 hooks 的文件型 Agent**（如未装 hooks 的 CC 只有转录目录可看）——限深限数防目录失控。
- 现有 `ClaudeCodeAdapter` 的偏移/cwd 缓存逻辑迁入引擎，CC 适配器瘦身为「glob + 行解析 + cost-state/标题特有逻辑」。

#### 2.3.2 SqliteTailEngine（现 ZCode 采集器机制泛化）

- 输入：`{ 库路径(支持环境变量重定向，如 KIMI_SHARE_DIR 类), 水位查询 SQL, 行映射回调, WAL 监视开关 }`。
- 机制：只读打开（现有 `OpenFlags` 策略）→ `started_at > 水位` 增量 → 行映射 → 水位推进（60s 安全余量 + 未来值钳制，逻辑沿用）。
- 监视：库文件 + `-wal` 文件 mtime 作为活动信号；连接复用策略（活跃会话每 tick 开新连接的开销实测评估，必要时改常驻只读连接 + `PRAGMA wal_autocheckpoint` 容忍）。
- **独立节流**：SQLite 打开比文件 stat 贵一个量级（WAL 大时更甚）。sqlite 档最多 6 家（ZCode/OpenClaw/Hermes/Copilot/Cursor/Windsurf），其扫描节流**独立于 FileTail**：活跃 2~5s、空闲 30s，不跟随 1s 快节奏——引擎按 `ScanBudget`（min_interval_ms）自律，调度器只负责唤醒。

#### 2.3.3 HookBridgeEngine（现 hook_events 机制泛化）

- 事件文件：按 Agent 隔离命名（`%LOCALAPPDATA%\AgentTrackerIsland\events\<agent>.jsonl`），桥脚本写自有文件，互不干扰；偏移/轮转机制沿用（含 total<offset 归零重读与 8MB 轮转）。
- **消费偏移键 per-agent 化（硬前提，归入 M2-3）**：现库设置键 `hook_events_offset` 为单键（service.rs），多 Agent 后必须迁移为 `hook_events_offset:<agent>`；一次性迁移：旧键值划归 `claude-code` 后删除旧键，迁移脚本走 store 迁移机制。
- **事件名映射表**（引擎内置，适配器可增补）：
  - 直通集：`UserPromptSubmit / PreToolUse / PostToolUse / Stop / SessionStart / SessionEnd / Notification / SubagentStart / SubagentStop`（CC/Codex/Kimi 同名同语义）
  - 差异集：Codex `Interrupt`→按 Stop 处理（留痕原始名）；Kimi `PostToolUseFailure/StopFailure`→**新增信号 `sig.last_failure`，喂给 error 判定**（比 CC 的通知文本启发式更精确，向后兼容）
  - 未知事件：透传给适配器声明的 fallback（现有逻辑：退启发式）
- **配置生成器扩展**（settings.json 注入器泛化）：按 Agent 生成并注入 hooks 配置——CC=JSON settings 注入（现有）、Kimi=TOML `[[hooks]]` 追加、Codex=按其配置格式（P1 装机核实后定）；桥脚本参数化 agent 标识，一份脚本源码编译进二进制（现有 `include_str!` 模式沿用）。
- 安装页：设置页 hooks 卡片从 CC 单卡片改为按 Agent 列表（已装/未装/一键装卸逐家独立）。

#### 2.3.4 可选通道（P2 后按需启用）

- **OtelSink**：优先支持「outfile 模式」（Gemini 遥测落文件，我们 tail 该文件，零端口零侵入）；OTLP gRPC receiver（localhost:4317，端口可配）仅作 opt-in 增强，默认关。两者都属红线④的增强档：Agent 未配置时走文件/启发式降级，不允许「未配置就不可用」。
- **LocalHttp**：订阅本地 Agent 的 push API（OpenCode `opencode serve` 的 `/event` SSE）。约束：只连本机回环、只订阅不指令（红线①：我们是订阅者不是控制者）、Agent 未开 serve 时静默降级（OpenCode/MiMo 降级到 SQLite 水位主通道，2026-09-23 通道反转后修订）。

### 2.4 调度器设计（实时性的落点）

分两步走，第一步与第二步共享同一骨架（`recv_timeout` 唤醒环）：

**阶段一（P0）：选择性快轮询，零新依赖，当天可上**

| 轮询对象 | 频率 | 成本 |
|---------|------|------|
| 快轮白名单：各适配器声明的「回合起点信号」（CC hook 事件文件、ZCode 当日日志 jsonl、未来各家的转录热点） | 恒定 1~2s，每项一次 `stat` | 微秒级 ×n，可忽略 |
| 全量采集 tick | 自适应：上轮快照含 working/waiting 会话→1s；全空闲→5~10s | 现有增量采集成本，空闲时 14 家全扫 <50ms 预算 |
| 进程探测 | 30s 缓存（不变） | 不变 |

效果：CC/ZCode「开始干活」的首信号延迟 ≤2s；活跃期数据刷新 ≤1s；空闲期开销近零。

**广播签名去重（归入 M2-1，1s 节拍下的必要配套）**：`generated_at` 每轮必变，若全量 emit，前端每秒无意义重渲染。规则：对 `sessions/island/quotas/today_*/degraded` 计算内容签名（`generated_at` 除外），与上轮一致则跳过 emit；前端逻辑零改动。

**阶段二（P0.5，P0 实测后决定是否上）：notify 事件驱动**

- 引入 `notify` crate（Windows 上是 ReadDirectoryChangesW，天然递归），watch 目标＝各适配器 `watch_paths()` 声明（转录 glob 的父目录、事件文件、sqlite 目录）。
- 事件 → 250ms 防抖 → 唤醒调度环立即 tick；慢 tick（10s）兜底保留，专管时间类状态迁移（看门狗降级、活动窗口过期、错误窗口过期）与额度刷新。
- 明确认知：轮询与监听都是业界正解（Filebeat/promtail 以轮询为核心机制），notify 是「更优雅」不是「更正确」；其防抖/事件风暴/目录重建重绑是真实复杂度，故列为实测后的升级项而非前提。

### 2.5 数据层（结论：schema 零迁移，做三项小补强）

| 项 | 现状 | 动作 |
|----|------|------|
| `usage_records/sessions/status_events` 的 agent 维度 | 已有列 + 幂等键含 agent | 无需迁移 |
| 水位表 | 已按 agent_id | 无需迁移 |
| 索引复查 | P1 起会话数×14 增长 | P1 接入第二家前 EXPLAIN QUERY PLAN 复查报表/会话中心重查询，按需补索引（迁移脚本走 store 现有迁移机制） |
| `provider_from_model` 启发式 | 全局函数，模型名前缀猜测 | 适配器可声明 provider 覆盖（如各家私有模型名），未声明走启发式 |

### 2.6 展示层多 Agent 改造（前端小改，架构不变）

| 项 | 现状 | 动作 |
|----|------|------|
| Agent 清单 | `AGENT_DEFS` 硬编码 2 家 | 每接入一家登记：id/中文名/默认色/图标（`shared/types.ts` + `shared/icons.tsx`）；设置页勾选列表与 `agents_enabled` 自动适应 |
| 胶囊主文案 | 按计数聚合（「N 工作中」） | 天然支持 N 家，不动 |
| 会话卡片/岛面板 | agent 徽标 + 颜色已数据驱动 | 不动 |
| **进程探测匹配** | `probe_processes` 硬编码 zcode/claude 名字匹配（service.rs） | **必须声明化**：适配器声明 `{ 进程名/命令行关键词, 排除项 }`，引擎统一执行——14 家接入的硬前提，归入 P0 |
| 会话列表容量 | 截断 100/90 天 | 每家独立截断配额（避免单家历史淹没他家），P1 落地 |
| 设置页 hooks 卡片 | CC 单卡片 | 泛化为按 Agent 列表（§2.3.3） |

### 2.7 状态可达性矩阵（「状态机不动」的边界说明）

状态机的判定逻辑不动，但各 Agent 的信号源决定了它**能到达的状态集合不同**——这是用户预期管理的核心事实，也是各家适配器交付时的验收基准：

| Agent | working | waiting | error | idle/offline | 依据 |
|-------|---------|---------|-------|--------------|------|
| Claude Code | ✅ hooks/启发式 | ✅ Notification | ✅ 限流消息/最近错误 | ✅ | 现状全量 |
| ZCode | ✅ 日志活动信号（P0 补强后） | ❌ 无信号源 | ✅ model_usage.error_type | ✅ | sqlite 档现状 |
| Codex | ✅ hooks | ✅ **PermissionRequest**（hooks 独有事件） | ⚠️ 有限——无通知类 hook，依赖 rollout 内错误痕迹（装机核实） | ✅ | hooks 12 事件 |
| Kimi Code | ✅ hooks | ✅ Notification | ✅ **StopFailure/PostToolUseFailure**（信号最精确） | ✅ | hooks 20 事件（新版 kimi-code，2026-09-23 核实） |
| OpenCode | ✅ db mtime 启发式（10a）+ SSE | ⚠️ SSE 权限类事件装机核实（10b） | ✅ message 行 error 字段（10a 即有） | ✅ | SqliteTail 主通道 + push API 增强档 |
| MiMo Code | ✅ 同 OpenCode（同族 10a） | ⚠️ 同上 | ✅ 同 OpenCode | ✅ | 同族参数化复用 |
| Gemini / Qwen | ✅ 启发式＋hooks 干活类事件 | ✅ hooks（Gemini=Notification 权限确认；Qwen=PermissionRequest） | ✅ Gemini=转录 error 行；Qwen=StopFailure/PostToolUseFailure（精确枚举） | ✅ | 文件 tail＋hooks（2026-09-23 纠正「无 hooks」预想，详 01-RESEARCH §13） |
| OpenClaw | ✅ session_nodes.status（running/done/failed/killed/timeout，库内现成）＋启发式 | ❌ 无权限类信号源 | ✅ 有限——transcript stopReason=error 弱信号（精确载体装机核实 §14.3） | ✅ | sqlite 档（2026-09-24 M2-13 核实） |
| Hermes | ✅ last_activity_at 单调启发式（快轮 state.db/-wal mtime）＋gateway_heartbeats 装机核实 | ❌ | ❌ **库内无逐调用错误载体**（api_request_error 仅 hooks 面；2026-09-24 M2-14 核实） | ✅ | sqlite 档（累计快照重采） |
| Copilot | ✅ 启发式 | ❌ | ⚠️ 库内错误字段装机核实 | ✅ | sqlite 档 |
| Cursor/Windsurf | ⚠️ 弱启发式 | ❌ | ❌ | ✅ 进程探测 | 实验性档 |

结论与约束：① waiting/error 的覆盖度是各家差异最大的维度，适配器交付时必须在 01-RESEARCH 勘察记录中写明本家可达状态集；② `SessionSignals` 需要为 Kimi 的 `StopFailure/PostToolUseFailure` 新增 `last_failure` 信号位（喂 error 判定，比通知文本启发式精确），这是状态机唯一的数据结构扩展，判定优先级逻辑不变。

### 2.8 隐私与安全细则（14 家接入的成文红线）

1. **绝不落内容**：所有适配器解析只提取元数据与会话 id/模型/token 数/时间戳/错误类型，对话文本、工具输入输出一律不进自库、不进日志。现有 CC 采集已是此模式，此处成文防止 14 家接入时走样（code review 检查项）。
2. **OTel outfile 的 logPrompts 陷阱**：Gemini 遥测默认 `logPrompts=true`，outfile 会包含 prompt 文本——OtelSink 解析只取 metrics 结构中的数字字段；设置页引导文案提示用户可将 `logPrompts` 关闭。
3. **网络通道绑回环**：OTLP receiver 与 LocalHttp（SSE）只监听/连接 `127.0.0.1`，端口可配，默认关（OTLP）或跟随 Agent 自身配置（SSE）。
4. **环境变量重定向不盲信**：`KIMI_SHARE_DIR`、MiMo 的 `MIMOCODE_CONFIG_DIR` 等重定向值须校验目录存在且可读，异常即按默认路径+留痕。
5. **hooks 注入必须用户显式同意**：沿用现有设置页一键装卸模式（有备份、可还原、防重复），泛化到多 Agent 时逐家独立开关，绝不静默注入。

### 2.9 降级矩阵（任何单源失败只影响该家）

| 故障 | 用户可见行为 | 留痕 |
|------|-------------|------|
| hook 事件文件不存在/被占用 | 该家降级启发式状态（增强档缺失≠不可用，红线④） | 静默（预期场景） |
| 转录目录不存在 | 该家不出现在面板（=未安装） | 静默 |
| SQLite 打不开/WAL 异常 | 该家本轮隐藏，下轮重试 | debug 留痕 |
| SSE 断连 | 指数退避重连，超限自动切文件 tail 通道 | info 留痕一次 |
| OTel 未配置/失败 | 启发式状态兜底 | 静默 |
| 单适配器解析 panic | catch_unwind 隔离，该家本轮跳过，线程续跑（现有机制） | error 留痕 |

---

## 3. 分期实施计划（M2 任务分解，按优先级逐项开工）

> 任务编号 M2-xx；开工时同步登记到 [03-TASKS](03-TASKS.md) 看板。
> 每项任务通用的验收底线：`cargo test` 全绿 + `test_real_*` 真实数据对账 + 宪法红线自查表过一遍。

### P0 实时性底座（改造自己，立骨架）——预计 1~2 个会话

| 任务 | 内容 | 验收 |
|------|------|------|
| M2-1 调度器骨架 | `spawn_aggregator` 的 `sleep` 改 `recv_timeout` 唤醒环；自适应 tick 间隔（活跃 1s/空闲 5~10s）；快轮白名单（HotSignal 两形态，§2.3.1）；**广播签名去重**（§2.4，1s 节拍配套） | ①手感：CC/ZCode 发消息后 ≤2s 岛变呼吸绿、回合结束 ≤2s 常亮（用日志时间戳验证信号→快照时延）；②性能：空闲 10 分钟 CPU <1%、内存无增长趋势；③前端每秒快照风暴消失（签名去重生效，DevTools 或日志验证） |
| M2-2 ZCode 活动信号升级 | `~\.zcode\cli\log\zcode-当日.jsonl` 与当日 rollout 文件 mtime 纳入 `last_activity_at`（2026-09-22 实测：日志在每次工具调用边界实时追加；注意按日轮转路径现算） | 长回答（>90s）生成期间状态保持 working 不掉 idle；跨零点会话日志切换无状态跳变 |
| M2-3 引擎抽取与迁移 | `FileTailEngine`/`SqliteTailEngine` 落地，CC/ZCode 适配器迁移为「声明+解析」，对外 `CollectOutput/SessionInfo/幂等键` 不变；**hook 消费偏移键 per-agent 化迁移**（§2.3.3：`hook_events_offset` → `hook_events_offset:<agent>`，旧键划归 claude-code） | ①升级前后 7 天真实数据逐分项对账一致（input/output/cache_read/cache_creation/调用次数）；②重启后水位/偏移行为不变；③hooks 事件消费在迁移后无重复无丢失 |
| M2-4 进程匹配声明化 | `probe_processes` 的名字匹配挪入适配器声明（`{ 进程名/命令行关键词, 排除项 }`） | 新增测试适配器（假进程名）可被引擎识别；现有 zcode/claude 判定结果与迁移前一致 |
| M2-5 P0.5 评估 | 开发者模式观察一周：**量化判据**——快轮档已达成 ≤2s 首信号延迟的前提下，仅当出现「空闲期 CPU 有感（>2%）」或「用户主观仍觉迟滞」之一才启动 notify 改造；否则 notify 降为 backlog | 结论与数据回写本文 §2.4 |

### P1 Codex + Kimi Code（富信号双响炮）——预计 2~3 个会话

| 任务 | 内容 | 验收 |
|------|------|------|
| M2-6 Codex 适配器 | rollout JSONL tail（`~\.codex\sessions\YYYY\MM\DD\`，注意按日期分目录的枚举）；token 字段名装机核实；hooks 装机核实配置格式后接入 HookBridgeEngine | `test_real_codex`：与 Codex 官方统计对账（分项 token）；hooks 装卸往返测试 |
| M2-7 Kimi 适配器 | `~\.kimi\sessions\**\{context,wire}.jsonl` tail；`KIMI_SHARE_DIR` 重定向支持；TOML hooks 注入器；`StopFailure/PostToolUseFailure` → `sig.last_failure` 新信号接入状态机 | `test_real_kimi` 对账；错误回合标红准确率优于通知文本启发式 |
| M2-8 前端登记 | `AGENT_DEFS` 新增两家（名称/默认色/图标）；设置页 Agent 勾选与 hooks 卡片列表化 | 岛/面板/会话中心/报表四端正确显示与筛选 |
| M2-9 索引复查 | 会话量翻倍前的 EXPLAIN 复查（§2.5） | 重查询无全表扫描 |

### P2 OpenCode + MiMo Code + Gemini + Qwen Code（push API、同族参数化与 OTel 首秀）——预计 3~4 个会话

| 任务 | 内容 | 验收 |
|------|------|------|
| M2-10 OpenCode + MiMo Code 适配器（同内核） | **2026-09-23 源码调研后拆两步（详 01-RESEARCH §12）**：主存储实为 SQLite（原定 JSON tail 通道不存在）→ **M2-10a（先行）**：`OpenCodeFamilyAdapter` 同族参数化（差异仅 agent id/数据根/db 文件名），只读 SQLite 水位通道（ZCode 同款），消息行 usage 解析+`msg_` id 幂等+error 行喂状态机；**M2-10b（装机后）**：LocalHttp SSE 实时增强档（opt-in，`session.next.*` 事件带 usage；端口发现/事件流实测为前提） | 10a：两家状态+用量全可用，同族适配器参数差异仅配置项，无复制粘贴代码；10b：SSE 订阅可观测（亚秒实时） |
| M2-11 Gemini + Qwen Code 适配器（同 fork 族） | Gemini：`~\.gemini\tmp\<hash>\chats\` + `logs.json` tail（无 hooks，走启发式）；OTel **outfile 模式**接入（设置页引导用户配置 `settings.json` 的 telemetry.outfile 指向自家目录，我们 tail）。Qwen Code：`~\.qwen\` 同构目录，**同一适配器换根路径参数直接覆盖** | 两家无 OTel 时状态可用（启发式）；配置 outfile 后用量/状态升级；Qwen 与 Gemini 共用解析代码 |
| M2-12 OtelSink 骨架 | outfile 轮询解析落地（OTLP receiver 留 backlog）✅ 2026-09-23：`collector/otel.rs` 公共模块＋两家适配器组合挂载（无新引擎管线——outfile 模式本质单文件 tail，独立引擎属过度设计，实施裁剪）；字段级调研详 01-RESEARCH §13.4；**token 行暂不入库防双计（与转录通道无公共 id 可对齐，所有者拍板），api_error 行入库走 recent_error**；装机对账后可一行切换主通道 | Gemini 用量与 Gemini CLI `/stats` 对账（`test_real_otel_*` 装机补跑，§13.3 清单） |

### P3 SQLite 档三家——预计 2 个会话

| 任务 | 内容 | 验收 |
|------|------|------|
| M2-13 OpenClaw | `openclaw-agent.sqlite` 水位采集（session rows 的 token counters + 转录树增量）——**2026-09-24 源码调研后落地**：多 agent 多库枚举＋三层会话结构（session_nodes/windows/transcript_events）＋事件 zstd 解压（01-RESEARCH §14），合成样本 7 单测 | 对账 + 状态可用；`test_real_openclaw` 装机补跑（§14.3 八项清单） |
| M2-14 Hermes | state.db 累计快照重采——**2026-09-24 源码调研后落地**：无逐调用流水表（总纲预想修正，01-RESEARCH §15），session_model_usage 行重采＋保留最大幂等＋task 列后台行＋多根枚举（default/profiles/env），合成样本 6 单测；hooks 不接（YAML+consent 成本，列增强档） | 对账 + 状态可用；`test_real_hermes` 装机补跑（§15.3 八项清单） |
| M2-15 Copilot CLI | `~\.copilot\session-state\` 文件 + sqlite store 二选一（以实测信息密度定） | 同上 |

### P4 逆向档收尾——预计 2 个会话 + 持续跟进

| 任务 | 内容 | 验收 |
|------|------|------|
| M2-16 WorkBuddy 勘察定档 | 装机核实落盘与 hooks；按结果归入 B/C/D 档并回写 §1.2 | 勘察记录回写 01-RESEARCH |
| M2-17 Cursor（实验性） | `state.vscdb` 只读监视；设置页标注「实验性：格式随版本漂移」 | 可观测即交付；失败静默降级不报错 |
| M2-18 Windsurf（实验性） | 同上策略 | 同上 |

### 接入 SOP（每家适配器的标准流程，P1 起固化）

1. **勘察**：装机核实目录/格式/事件，记录到 01-RESEARCH「各 Agent 数据面勘察」节（含核实日期）；
2. **适配器**：声明（glob/库路径/事件映射/进程匹配）+ 行解析 → `UsageRow`；
3. **hooks**（若有）：桥脚本参数化 + 配置注入器 + 装卸往返测试；
4. **对账**：`test_real_<agent>` 与该家官方统计口径比对（分项 token）；
5. **前端登记**：`AGENT_DEFS`/颜色/图标；
6. **状态精度说明**：按 §2.7 写明本家可达状态集，回写勘察记录；
7. **窗口跳转**（可选，体验增强）：`focus_session` 的窗口标题/类名匹配适配（CLI 家匹配终端窗口标题、IDE 家匹配窗口路径），做不了则点击跳转对该家禁用并置灰，不留假按钮；
8. **回写**：§1.2 矩阵行 + 03-TASKS 勾销。

---

## 4. 宪法红线对照表

| 红线 | 本方案符合性 |
|------|-------------|
| ① 纯只读 | 全部通道是文件只读/SQLite 只读/SSE 订阅/接收 Agent 主动推送的遥测；无任何请求转发或注入 |
| ② 故障隔离 | 桥脚本继续只写本地事件文件不连端口；引擎读失败一律静默降级本轮跳过；OTLP/HTTP 通道默认关且 Agent 侧无感知 |
| ③ 顺序无关 | 字节偏移/水位增量补录机制跨引擎沿用；先装 Agent 后装岛照常回溯 |
| ④ 渐进降级 | hooks→文件→进程的三级信号模型不变；OTel/SSE 是增强档，未配置走启发式，无「未配置就不可用」 |
| ⑤ 不抢焦点 | 本方案不涉及窗口行为，现状保持 |

---

## 5. 风险与对策

| # | 风险 | 对策 |
|---|------|------|
| 1 | **各家格式随版本漂移**（最大长期成本，逆向档尤甚） | 01-RESEARCH 勘察记录带核实日期；`test_real_*` 对账测试做回归防线；解析失败计数留痕（现有机制）保证「静默归零」可发现；跟进社区逆向 registry（deja-vu 等） |
| 2 | Cursor/Windsurf 无格式承诺 | 「实验性」明示 + 失败静默降级；预期管理写进设置页文案 |
| 3 | Agent 数量增长推高轮询成本 | 快轮白名单制度（只快轮「回合起点信号」）；自适应间隔；P0.5 notify 终态进一步降 |
| 4 | hooks 注入与用户既有配置冲突 | 沿用现有备份/防重复/原子写/卸载还原机制；按 Agent 隔离桥脚本与事件文件 |
| 5 | OTLP 端口被占 | 默认 outfile 模式（零端口）；receiver 仅 opt-in 且端口可配 |
| 6 | 14 家一步到位的冲动导致烂尾 | 本文分期即合同：每家独立可交付、独立验收；未开工的家不阻塞已交付的家 |
| 7 | 多 Agent 并发写自库的锁竞争 | 引擎仍在单一聚合线程内执行（现架构），无并发写；若未来引擎并行化，先过 store 的写队伍评估再动 |

---

## 6. 与既有文档的关系

- [02-DESIGN](02-DESIGN.md) §1「采集调度」行更新为：P0 选择性快轮询（本文 §2.4 阶段一），notify 为 P0.5 升级项；
- [03-TASKS](03-TASKS.md)：M2 任务序列开工时逐项登记（M2-1 ~ M2-18），看板规则不变；
- [01-RESEARCH](01-RESEARCH.md)：新增「各 Agent 数据面勘察」节，承接 §1.2 矩阵的逐家细节与后续回写；
- [HANDOFF](HANDOFF.md)：每会话结束按 WORKFLOW 惯例更新进度。

---

## 附录 A：核心接口草案（P0 实现锚点，最终签名以实现期为准）

> 以下为 Rust 伪码级草案，展示「声明与机制分离」的落点形状；实现时可按编译器反馈微调，
> 但三条不变量必须守住：①适配器不含轮询/监听循环；②引擎不含 per-agent 分支；③`CollectOutput`/幂等键结构不变。

```rust
/// 快轮信号（调度器 1~2s 高频探测的目标；探测命中才唤醒全量 tick）
pub enum HotSignal {
    /// 单文件 stat（hook 事件文件、当日日志——路径可随日期变化，闭包现算）
    File(Box<dyn Fn() -> Option<PathBuf> + Send + Sync>),
    /// 目录枚举取 max(mtime)（未装 hooks 的文件型 Agent），限深限数防失控
    DirScan { glob: String, depth: u8, max_files: usize },
}

/// 文件型采集源声明（FileTailEngine 的输入）
pub struct FileSource {
    pub glob: String,                       // 支持 ** 递归（Codex 日期分区目录）
    pub parse_line: LineParser,             // 行解析回调（适配器私有知识）
    pub hot: Option<HotSignal>,             // 回合起点信号（None=不参与快轮）
}

/// SQLite 型采集源声明（SqliteTailEngine 的输入）
pub struct DbSource {
    pub path: Box<dyn Fn() -> Option<PathBuf> + Send + Sync>, // 支持环境变量重定向
    pub watermark_sql: &'static str,        // 水位增量查询
    pub map_row: RowMapper,                 // 行 → UsageRow
    pub min_interval_ms: i64,               // 独立节流（2~5s 活跃 / 30s 空闲）
}

/// AgentAdapter 扩展声明（全部带默认实现——适配器只声明自己有的东西）
pub trait AgentAdapter: Send + Sync {
    // —— 现有（不变）——
    fn id(&self) -> &'static str;
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>>;
    fn collect_usage(&self, watermark_ts: i64) -> anyhow::Result<CollectOutput>;
    // —— 新增声明（默认空/无）——
    fn file_sources(&self) -> Vec<FileSource> { vec![] }
    fn db_sources(&self) -> Vec<DbSource> { vec![] }
    fn process_match(&self) -> Option<ProcessMatch> { None }  // 进程名/命令行关键词 + 排除项
    fn provider_override(&self, model: &str) -> Option<String> { None }
}

/// 调度器主环（P0：纯轮询；P0.5：notify 事件同样走 poke 通道，结构不变）
fn scheduler_loop(...) {
    loop {
        let deadline = next_tick_deadline();          // 活跃 1s / 空闲 5~10s
        let poked = rx.recv_timeout(deadline).is_ok();// 快轮线程/notify 唤醒
        // 各引擎按自身 ScanBudget 自律：未到间隔直接跳过，不做无谓扫描
        if poked || due(deadline) {
            let snap = catch_unwind(|| aggregate_all());
            if snapshot_signature_changed(&snap) {   // 广播签名去重（generated_at 除外）
                let _ = app.emit("island-snapshot", &snap);
            }
        }
    }
}

/// 快照内容签名：sessions/island/quotas/today_*/degraded 参与哈希，
/// generated_at 不参与——内容未变不 emit，前端零重渲染
fn snapshot_signature(snap: &IslandSnapshot) -> u64 { /* serde 转后哈希剔除时间戳字段 */ }
```

### A.1 P0 文件级改造清单（预期触点）

| 文件 | 改造 |
|------|------|
| `src-tauri/src/collector/mod.rs` | 新增 `FileSource/DbSource/HotSignal/ProcessMatch` 声明类型与 `AgentAdapter` 默认方法 |
| `src-tauri/src/collector/engine.rs`（新建） | FileTailEngine/SqliteTailEngine：枚举（glob 递归+目录 mtime 剪枝）、偏移读、水位查询、节流自律 |
| `src-tauri/src/collector/{claude_code,zcode}.rs` | 瘦身为声明+解析，机制逻辑迁出 |
| `src-tauri/src/state/service.rs` | Aggregator 消费引擎产出；`hook_events_offset` per-agent 键迁移；快轮消费逻辑 |
| `src-tauri/src/lib.rs` | `spawn_aggregator` → 唤醒环 + 快轮线程 + 广播签名去重 |
| `src-tauri/src/store/mod.rs` | 无 schema 变更；仅设置键迁移脚本（偏移键） |
| 测试 | 引擎单测（枚举/偏移/节流/签名）+ `test_real_*` 对账回归 + 迁移前后 7 天数据对账脚本 |
