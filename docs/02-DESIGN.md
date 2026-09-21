# AgentTrackerIsland 实现方案（02-DESIGN）

> 状态：**v1.0 定稿，回写至 2026-09-17**（基于 [00-REQUIREMENTS v1.0](00-REQUIREMENTS.md) + [01-RESEARCH](01-RESEARCH.md)）
> 定稿：2026-09-16 | 技术栈与架构经阶段 2 调研确认，实施期改动须回写本文档

## 1. 技术栈（定稿，含落地回写）

| 层    | 选型                                             |
| ---- | ---------------------------------------------- |
| 框架   | Tauri 2(stable)                                |
| 后端   | Rust（edition 2021，Cargo.toml 实际值）             |
| 前端   | React 19 + TypeScript + Vite                   |
| 存储   | rusqlite（bundled）——自库读写 + ZCode 库只读            |
| 采集调度 | 定时轮询水位增量（10s tick；notify-rs 文件监听未引入，M1 视需要评估） |
| 窗口效果 | 窗口全透明 + CSS 自绘背景（window-vibrancy/Acrylic 已弃用——窗口级效果会把整个矩形染灰破坏胶囊形态，T8 定论；依赖保留备 M1 全宽形态） |
| 窗口激活 | windows-rs(SetForegroundWindow)                |
| 分发   | 便携 zip（release 产物）——仅所有者宣布正式发布时执行（NSIS 同，见 WORKFLOW 构建打包纪律） |

## 2. 架构与模块

```
┌────────────────── WebView(React) ──────────────────┐
│ IslandApp:收缩态 │ 展开面板 │ 设置页(内嵌简单表单) │
└──────────────┬─────────────────────────────────────┘
        Tauri command(event 推送 state_changed / quota_updated)
┌──────────────┴─────────────────────────────────────┐
│ Rust 核心(后台线程)                                 │
│ ┌───────────── Agent 适配器(trait)─────────────┐   │
│ │ zcode.rs:只读 db.sqlite 水位采集+状态推断    │   │
│ │ claude_code.rs:JSONL 解析+hooks 事件消费     │   │
│ ├───────────── Provider 适配器(trait)─────────┤   │
│ │ glm.rs:Monitor API→quota_snapshots          │   │
│ ├───────────── 状态聚合器 ────────────────────┤   │
│ │ 三级融合+看门狗(5min)+error 判定→会话状态   │   │
│ └───────────── store(SQLite 自库)─────────────┘   │
└─────────────────────────────────────────────────────┘
```

### 2.1 AgentAdapter trait（落地签名）

```rust
pub trait AgentAdapter: Send + Sync {
    fn id(&self) -> &'static str;                       // "zcode" | "claude-code"
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>>;          // 发现会话
    fn collect_usage(&self, watermark_ts: i64) -> anyhow::Result<Vec<UsageRow>>; // 水位增量
}
```

> 📌 回写（2026-09-17）：原设计的实时事件源 `watch()` 未实现——M0 采集模型为
> "定时轮询水位增量"（聚合器 10s tick 驱动），watch 推迟为 M1 优化项
> （见 src-tauri/src/collector/mod.rs 头注）。

**zcode.rs（调研定论：纯只读）**

- 打开 `~\.zcode\cli\db\db.sqlite`（readOnly），按 `model_usage.started_at` 水位增量拉取
- 列白名单读取（provider_id/model_id/agent/mode/task_type/status/started_at/completed_at/
  duration_ms/time_to_first_token_ms/input_tokens/output_tokens/reasoning_tokens/
  cache_creation_input_tokens/cache_read_input_tokens/error_type/retry_count/session_id/turn_id）
- 状态推断：工作=最近 usage 在 90s 内 或 session.time_updated 活跃；错误=最近记录 error_type 非空
- 会话标题/目录：join `session` 表

**claude_code.rs**

- 扫描 `~\.claude\projects\**\*.jsonl`，解析 assistant 消息的 `message.model` + `message.usage`
- 幂等键：（session_id， 消息时间戳， model）；同键冲突时仅当新行四项用量合计更大才
  整行覆盖（保留最大快照，与流式去重同口径，2026-09-17 回写）
- 增量：按文件 mtime 过滤（≤水位跳过）+ 全量重读逐行时间过滤；"文件内偏移量"
  未实现（每 tick 重读有变动的文件，M0 规模可接受，列入看板待议区）
- hooks 事件（增强档）：消费 hook-bridge 写的事件文件（协议见 §4）

### 2.2 ProviderAdapter trait

```rust
pub trait ProviderAdapter: Send {
    fn id(&self) -> &'static str;           // "glm"
    fn fetch_quota(&self) -> anyhow::Result<Vec<QuotaSnapshot>>; // 5h+weekly 两条
}
```

**glm.rs**：`GET {base}/api/monitor/usage/quota/limit`，Authorization 裸 key，5s 超时；
字段容错解析（usage/remaining/currentValue/total/percentage/nextResetTime）；
刷新：5min 定时+启动；失败降级显示最近快照。base/key 来自设置
（默认读环境变量 ANTHROPIC_AUTH_TOKEN 与 ANTHROPIC_BASE_URL）。
> 📌 M0 落地回写（2026-09-17）：刷新实现为"定时 5min + 启动即拉"两路，"手动刷新"
> 未做（5min 粒度自用足够，所有者 2026-09-17 拍板），M1 报表页一并考虑。
> 📌 M1 回写（2026-09-17，审查修复）：凭据优先级实现为"应用设置（token 非空才
> 生效）> 环境变量 > claude-menu suppliers.json"——设置 Key 留空不写入、自动回落
> 发现链（兑现设置页"留空则继续沿用"）；凭据来源写入 app_settings.glm_token_source
> 供设置页展示。百分比兜底仅接受 usage/remaining 换算，不可换算返回 None
> （UI 显示 "--"），不拿 currentValue 绝对量冒充百分比。

### 2.3 状态聚合器

- 每会话状态机：working ⇄ idle；waiting；error；offline（原设计的 online 态
  无产出路径，M1-7 移除）
- 事件优先级：hooks 事件 > 文件/usage 时间启发式 > 进程存在性
- 看门狗：working >5min 无新事件/新 usage → idle；error 不受看门狗影响
- error 判定：hooks Notification 含限流关键词（§7 正则）/ ZCode error_type 非空
  > 📌 M0 落地回写（2026-09-17）：卡片仅显示"出错"态，不展示原因明细；`state_reason`
  > 字段已预留，DB 写 NULL。原因诊断（额度耗尽/进程退出/API 报错）M1 诊断页一并做。
  > 📌 M1-6 回写（2026-09-17）："额度快照=100%"不再写入 error 判定/会话状态——
  > service 层产出快照级 quota_exhausted 标志，前端驱动胶囊变红/贴边标签红光/
  > 额度弧线红，会话状态保持真实值。
- 聚合岛收缩态：任一会话 error→红；否则任一 waiting→琥珀；否则任一 working→
  呼吸绿；否则常亮绿；无会话→灰
- 静默提醒：额度 ≥阈值（80/95 可配）→ 收缩态额度文字变琥珀/红，不弹窗不出声

## 3. 自库 SQLite Schema(migrations/0001_init.sql)

```sql
CREATE TABLE sessions(
  id TEXT PRIMARY KEY,            -- "{agent}:{sessionId}"
  agent TEXT NOT NULL, provider TEXT, model TEXT,
  project_dir TEXT, title TEXT,
  first_seen_at INTEGER, last_seen_at INTEGER,
  state TEXT DEFAULT 'offline', state_reason TEXT
);
CREATE TABLE usage_records(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL, agent TEXT NOT NULL,
  model TEXT NOT NULL, provider TEXT,
  ts INTEGER NOT NULL,
  input_tokens INTEGER, output_tokens INTEGER,
  reasoning_tokens INTEGER, cache_read_tokens INTEGER, cache_creation_tokens INTEGER,
  duration_ms INTEGER, ttft_ms INTEGER,          -- ZCode 独有,Claude Code 置 NULL
  error_type TEXT,
  UNIQUE(agent, session_id, ts, model)
);
CREATE INDEX idx_usage_ts ON usage_records(ts);
CREATE TABLE quota_snapshots(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  provider TEXT NOT NULL, window_kind TEXT NOT NULL,   -- '5h'|'weekly'
  used_percent REAL, used_tokens INTEGER,
  reset_at INTEGER, fetched_at INTEGER NOT NULL
);
CREATE TABLE status_events(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  agent TEXT, session_id TEXT, hook TEXT, payload TEXT, ts INTEGER NOT NULL
);
CREATE TABLE watermarks(
  agent TEXT PRIMARY KEY, last_ts INTEGER, last_offset INTEGER
);
CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT);
```

数据清理（设置项）：按 `usage_records.ts`/`quota_snapshots.fetched_at`/
`status_events.ts` 滚动删除（启动时按周期执行一次），
周期：1 年（默认，2026-09-17 审查改，原"永不"）/3 年/2 年/1 年/6 月/3 月/1 月/1 周/永不。
`sessions` 表不在清理范围（每会话一行，增长极慢，列入看板待议区）。

## 4. hook-bridge 事件协议（Claude Code 增强档）

- 事件文件：`%LOCALAPPDATA%\AgentTrackerIsland\events\claude-code.jsonl`（append-only；2026-09-17 项目更名后路径，旧 AgentTracker 目录数据留在原地未迁移）
- 安装：设置页"启用精确状态"按钮→向 `~\.claude\settings.json` 的 hooks 注入
  SessionStart/UserPromptSubmit/PreToolUse/PostToolUse/Stop/Notification/SessionEnd
  各一条：`node "<home>\.claude\hooks\hook-bridge.js"`（async，注入 timeout 10s，
  桥脚本自带 2s 兜底退出）
  注入采用**读取-合并-写回**且先备份 settings.json.bak；卸载=精确移除自己注入的条目
- hook-bridge.js:stdin 读 JSON → append `{"ts":...，"hook":...，"session_id":...，
  "tool_name":?，"message":?}` → 退出
  （不连端口不找进程；主程序死活无关——红线②）
  > 📌 审查回写（2026-09-17）：事件文件消费改为 seek 增量读（不再整文件进内存）；
  > 修复"文件重建后 total<offset 导致消费永久失明"bug；新增 8MB 轮转
  > （已消费完才滚动为 `.jsonl.old`，偏移归零）。详见 §8-②。
  > 📌 T6 实测回写（2026-09-16）：事件名不带命令行参数，从 stdin 的
  > `hook_event_name` 读取；stdin 无 model 字段，协议已去 model（模型信息由 T4
  > 转录解析补齐）；桥脚本落盘位置为 `~\.claude\hooks\`（单一已知位置，用户可审计）
- ⚠️ T6 开工第一步：先装诊断 hook 打印真实 stdin 字段（文档站不可达，以实测为准）

## 5. 灵动岛窗口规格（Tauri）

| 项         | 配置                                                                                         |
| --------- | ------------------------------------------------------------------------------------------ |
| 主窗 island | decorations:false， always_on_top， skip_taskbar， transparent， resizable:false， shadow:false |
| 位置        | 默认顶部居中（计算 workArea）；拖拽后坐标存 app_settings                                                    |
| 效果        | 窗口全透明 + CSS 自绘背景（Acrylic 已弃用，见 §1 回写）                                   |
| 收缩态       | 高~48px 胶囊：状态灯 + "现在时"主文案（活跃计数/空闲+总数）+ GLM 最紧张窗口额度% + 今日 token（口径详见 §5 末展示规范）                     |
| 展开态       | hover 展开：今日汇总条 + 会话列表（活跃区/历史区折叠）+ 额度区（5h/周双条+倒计时）；卡片两行=标题+状态·时间 / 模型·项目徽章+token；**高度随内容自适应**（Panel 上报自然高度，上限 expanded_h 超出内部滚动，下限 240；窗口必须同步收缩——常驻顶层窗口若只缩内容不缩窗口，透明区会拦截下层应用点击） |
| 展开 focus  | non-activating：展开不调 set_focus,pointerLeave 收起（红线⑤）                                         |
| 托盘        | 右键菜单：显示/隐藏灵动岛、设置…、退出（左键同弹菜单）                                                                    |
| 状态灯       | working=呼吸绿（CSS animation）；idle/done=常亮绿；waiting=琥珀；error=红（微脉冲）；offline=灰                 |

> 📌 M0 落地回写（2026-09-17）：托盘落地为三菜单项（显示/隐藏灵动岛、设置…、退出），
> "暂停监控"未纳入（冻结需求未要求，所有者拍板不做）；左键同样弹出菜单。
> M1-1 追加"报表…"入口，现共四菜单项。
>
> 📌 M1-8 托盘与边角优化（2026-09-18）：菜单双分隔线分三组——岛操作（显示/隐藏）｜
> 开窗（报表/设置/关于）｜退出，操作逻辑不同的组各自归拢；"灵动岛"项文案动态化；
> tooltip 更名"去你的岛（AgentTrackerIsland）"。托盘图标暂不处理（所有者拍板；现状=ico 首条目
> 32×32，200% 屏 1:1，其他 DPI 由系统缩放，如需清晰化再按 DPI 选档内嵌 16/24/32 PNG）。
>
> 📌 托盘显隐语义重构（2026-09-18 二轮，所有者确认）：菜单项简化为"显示/隐藏"（纯窗口
> 可见性判定文案——胶囊态与贴边标签态都算可见），其余项去"…"。**"隐藏"=彻底消失**：
> 无论胶囊还是贴边标签，一键整窗 hide（典型场景：玩游戏前快速清屏，监控不断）；
> **"显示"=临时召唤**：先以胶囊态亮出位置（贴边标签太小，用户未必记得贴在哪），
> 若停靠边缘且自动隐藏开启，3s 后自动滑出收回（island-summon 事件→前端定时器，
> 鼠标移入取消，移出后交给常规移出滑出逻辑）；自由摆放则常驻不自动收。
> ⚠ **锁纪律（2026-09-18 死锁教训，两轮定位）**：`std::sync::Mutex` 非重入——
> `MutexGuard` 实现 Drop 会活到**函数末尾**（不是最后使用处），凡函数尾部持有
> motion 锁时**禁止调用任何会再次 lock motion 的函数**（如 refresh_toggle_text），
> 否则同线程自锁、motion 永久被持有，主线程 Moved 事件处理器拿锁阻塞 → 全 UI
> 冻结（贴边区变黑+托盘菜单消失+进程假死，聚合线程仍存活）。写法：先在作用域块内
> 取值放锁，再调下游。另：托盘菜单回调在主线程事件循环内，转后台线程执行是
> 主线程减负的卫生习惯（非死锁修复，真因见上）。
>
> 📌 M1 追加落地（2026-09-17，所有者追加需求）：岛支持自由拖拽 + 贴边自动隐藏——
> 拖放到屏幕上/左/右边缘 24px（逻辑）内自动吸附，吸附后滑出屏外仅留独立信息标签
> （顶部=胶囊底部 1/5 短条 14px，左右=半圆 D 形伸出 20px），鼠标移入滑入显示；
> 设置项 island_autohide 默认开启。交互细节：拖拽防抖 180ms + 左键检测，滑动动效
> 约 200ms，吸附优先级 上>左>右（实现见 lib.rs IslandMotion，几何纯函数有单测）。
> **岛宽自适应**：显示器逻辑宽 × 30%，夹取 [380， 800](island_width；比例参考三分律
> 1/3 与黄金分割小段 0.382 之间的主流悬浮组件区间，所有者笔记本 +1/5 手感校准)；
> Rust 贴边几何与前端渲染经 island_metrics command 共用同一结果。全链路使用逻辑坐标系。
> **悬停/点击展开**：设置项 hover_expand（默认开）——开启时悬停岛即展开信息卡片；
> 关闭时需点击岛展开、再点收回，移出岛仍自动收起；设置变更经 hover-expand-changed
> 事件实时推送岛窗口。
> **Agent 身份色与选择（设置项 agents_enabled / agent_colors）**：颜色 = Agent 身份
> （ZC 绿 / CC 橙 / Codex 蓝 / Claude Desktop 紫，设置页可自定义，重复颜色保存拦截）；
> 隐藏态等宽分段与面板徽标共用同一身份色；状态用亮度/动效表达（工作中 4s 慢呼吸/
> 等待 1.2s 快闪/出错红圈+快闪/空闲 45% 暗淡/离线近隐没）。勾选才采集/监控/展示
> （Aggregator 跳过未勾选 Agent 的扫描与采集，设置键缺省=全启用）；额度耗尽不改写
> 会话状态，由快照 quota_exhausted 驱动胶囊变红/弧线红/红光边框。
>
> 📌 **信息展示规范（2026-09-18 展示改造，所有岛 UI 迭代必须遵守）**
>
> 四条展示原则（调研 ccusage / Claude-Code-Usage-Monitor / 灵动岛 HIG 提炼）：
> 1. **数字必带口径**：每个数字要么自带归属（"今日""本会话""已用"），要么 hover 有
>    tooltip 说明口径；缺失值必须解释原因，不允许裸"--"
> 2. **现在优先**：胶囊与面板首屏回答"现在怎么样"，历史数据折叠下钻
> 3. **颜色语义唯一**：绿=正常/工作中，琥珀=等待/额度≥warn，红=出错/额度≥danger，
>    灰=无数据/离线/历史；身份色只回答"这是谁"，永不兼职表达状态
> 4. **逐层下钻**：胶囊（1 秒）→ 面板（10 秒）→ tooltip/报表页（深入），
>    每层是上一层的展开而非重复（HIG 收缩态/展开态信息连续性）
>
> 落地要点（与代码对应）：
> - **胶囊**："现在时"主文案（`N 出错 · N 等输入 · N 工作中`，只列非零项，状态词
>   全行不重复；无活跃时空闲+会话总数）；额度自动选**最紧张窗口**（用量%最高，
>   修"周快满胶囊全绿"盲区）；token 展示**今日**口径（账单口径含缓存，快照
>   today_tokens/today_calls）；额度未配置/查询不可用显灰字可见（红线④）；
>   token 口径=四项相加与账单一致（input+output+缓存读+缓存写）
> - **面板**：今日汇总条（活跃数/今日 token/调用次数 + 报表入口，show_report_window
>   command）；会话分**活跃区/历史区**，历史区默认折叠一行摘要；卡片两行布局
>   （第一行=状态·相对时间+会话标题主文案，第二行=模型/项目徽章+token）；
>   空闲超 2h 或进程退出按"已结束"展示（前端近似修正，根治需会话级进程归属）；
>   出错透出 error_type 中文原因；token tooltip 四项拆解
> - **隐藏态**：全部会话离线的 Agent 不渲染分段；额度线同"最紧张窗口"策略；
>   出错叠加"！"符号（顶条）/红点（左右圆心）；**明确不做 tooltip**——鼠标悬停
>   即触发 island_peek 滑入、本组件卸载，任何悬停提示没有展示时机（伪需求，
>   2026-09-18 所有者确认，勿再实现）
> - **图标**：一律单色 SVG（currentColor），禁用彩色 emoji（Windows WebView2
>   渲染彩色 emoji 破坏暗色科技风），见 src/shared/icons.tsx
> - **悬浮气泡（Tooltip）规则（2026-09-18 定稿，全项目通用——后续新页面/新需求
>   遵循同一套，不再逐次讨论）**：
>   ① **双轨边界按"窗口能否容纳自绘气泡"划分**：展开面板/设置/报表等常规窗口
>   一律用自绘 Tip 组件（src/shared/Tip.tsx）；胶囊收缩态（48px 高）、贴边隐藏态
>   等小微窗口用原生 title（OS 级渲染可越出窗口边界，样式不可控是接受此妥协的
>   已知代价）；**同一窗口内禁止两套混用**（样式与延迟必须一致，2026-09-18 曾因
>   面板混用两套被用户指出）；
>   ② **自绘 Tip 实现约定**：portal 挂 body（防滚动容器 overflow 裁剪）；
>   水平跟随光标＋垂直锚定触发元素（下缘优先，放不下翻转上缘）；渲染后
>   useLayoutEffect 量真实宽高再夹取视口，**禁止假设尺寸常量**（2026-09-18 漂移
>   事故教训：假设半宽 140px 导致贴边元素气泡偏移）；cloneElement 注入事件
>   不包裹额外 DOM（触发元素布局零侵入）；仅悬停时挂载节点（空闲零 DOM，
>   列表每行挂 Tip 的大规模场景安全；若实测卡顿再做单例气泡优化，当前 YAGNI）；
>   400ms 延迟防误触；
>   ③ **tooltip 只承载补充信息，不承载唯一关键信息**：被截断字段须有省略号、
>   缺失值须有"--"占位等**可见暗示**，不允许出现"不看气泡就看不懂"的字段

## 6. 设置页（内嵌 WebView 路由 #settings；2026-09-17 布局与交互重构）

- **交互模型：设置项即时生效**——改动即存即落地（去除"保存"按钮与保存后自动关窗）；
  文本输入（Key/阈值）失焦提交，Enter 等价失焦；hooks/自启本就点击即生效，心智统一
- **布局：分区卡片 + 统一设置行**——分区头为主标题 + 副标题**同行**（基线对齐，
  均不换行，窗口最小宽度按最宽一条副标题实测文字宽度设下限）；
  每行 = 固定标题（+状态徽标）+ 固定描述 + 右侧控件，文案不随选中态变化
  （遵循 Fluent 开关文案规范）；五节按使用频率排列：
  通用（主题、开机自启）→ 灵动岛（贴边隐藏/悬停展开）→ Agent 监控（勾选+身份色，
  自灵动岛节拆出，未勾选行颜色置灰禁用，附"恢复默认颜色"）→ 额度与凭据
  （平台/API Key/提醒阈值）→ 数据与维护（保留时长 7 档按时长降序：12 个月（默认）/6 个月/
  3 个月/1 个月/15 天/7 天/1 天，旧档位存储值回落默认/Claude Code hooks）；
  校验错误就近显示在出错行正下方（颜色撞色/阈值非法），操作结果用顶部 toast
  （成功 2.5s 自动消失，失败常驻）；"重启后生效"的事实（凭据/阈值）写入分区副标题
- 各节要点：GLM 平台（bigmodel/z.ai）+ key（密码框不回显、眼睛切明文、保存后清空输入框，
  留空=沿用已存/自动发现链不覆盖，来源徽标展示 glm_token_source）；
  额度提醒阈值（80/95，琥珀须小于红色，前置色点标识）；
- 外观（M1-4）：主题三档分段控件 system（默认）/dark/light，点击即存并 emit theme-changed
  广播（含本窗口，点击即预览），岛/设置/报表三窗口即时切换；
  跟随系统经 WebView2 PreferredColorScheme→matchMedia 感知，零 Rust 参与；
- 灵动岛（M1-6）：贴边自动隐藏开关、悬停/点击展开开关（CSS 滑动开关控件）；
  settings/report 窗口原生标题栏颜色跟随系统，不随主题选择（内容区跟随）；
- 色板（M1-4 变量化基础上新增语义色）：`--accent`/`--danger`/`--switch-off`
  分主题定义，深浅双主题下保证开关、勾选、徽标与错误提示的对比度；
- 界面文案一律使用中文标点（，：；（）），英文专有名内部符号与「/」「%」除外；
- 窗口：800×600（4:3 横向），可调大小（min 520×480，宽下限含最宽副标题实测 395px
  与行内文字可读缓冲）。

## 7. 风险与对策（实施期）

| 风险                     | 对策                              |
| ---------------------- | ------------------------------- |
| ZCode Schema 随版本漂移     | 列白名单+未知列忽略；scan 失败静默降级为"仅进程监控"  |
| Claude Code JSONL 格式变化 | 同上；对账任务（A2）作为回归项                |
| GLM API 变更             | 集中 glm.rs；快照降级展示                |
| Acrylic 在部分驱动下闪烁       | 设置项可关毛玻璃，退纯色                    |
| settings.json 与其他工具竞争写 | 写前备份+原子写（temp+rename）；冲突时提示用户手查 |
| hooks stdin 字段与预期不符    | T6 诊断 hook 实测先行                 |

## 8. 全面审查优化回写（2026-09-17，第五次会话）

> 页面样式与功能初验通过后，按"架构/框架/数据安全/性能"四维全面审查，以下改动
> 已全部落地并通过 cargo test 23/23 + npm run build；**待所有者手动验收**。

| # | 级别 | 改动 | 落点 |
|---|------|------|------|
| ① | 🔴 | **可观测性**：新增 `logging.rs`（log 门面+std 文件后端，`%APPDATA%...\logs\agenttrackerisland.log`，1MB 滚动；`AT_LOG=debug` 开调试级）+ 全局 panic 钩子；聚合线程 tick 包 `catch_unwind`（单轮 panic 不再杀线程致岛静默冻结）；**release 去 `panic="abort"`**（否则 unwind 捕获失效）；采集/额度/写库失败全部留痕 | logging.rs、lib.rs、store、glm |
| ② | 🔴 | **hook 事件文件**：seek 增量读（成本 O（新增））；修复 total<offset 永久失明 bug（重建当轮归零重读）；8MB 轮转 `.jsonl.old`（仅已消费完） | hook_events.rs、service.rs |
| ③ | 🔴 | **CC 转录增量读**：per-file 字节偏移缓存（64KB 回退量配对 60s 水位余量），mtime 未变直接跳过；cwd 头部提取按 mtime 缓存（原来每 tick 每文件重读 8KB） | claude_code.rs |
| ④ | 🟡 | **数据安全**：get_settings 不再下发 `glm_token`（以 `glm_token_set` 布尔位代替）；set_setting 键白名单；**CSP 启用**（原 null；style 允许内联，connect 含 ipc 与 dev HMR ws）；移除零引用的 tauri-plugin-opener（依赖/权限/前端包）+ 未用的 set-position 权限；hooks 原子写 rename 失败重试 3 次+报错指引，备份只留最近 5 份 | lib.rs、tauri.conf.json、capabilities、claude_code.rs |
| ⑤ | 🟡 | **性能**：报表四命令改 async+spawn_blocking（原同步 command 跑主线程，大数据量冻结全部窗口）；会话 token/模型批量查询（消 N+1,100 会话原为每 10s 200 次无索引扫描）；迁移 0002 加 `idx_usage_session` 索引并删除未用的 watermarks.last_offset 列；sysinfo System 复用+进程枚举降频 30s；GLM Client 复用（连接池/TLS 会话） | lib.rs、store、service、glm |
| ⑥ | 🟡 | **可靠性**：store/适配器 Mutex 中毒自恢复；insert_usage 事务失败改日志+放弃本轮（下轮重采兜底）；快照新增 `degraded` 标志（连续 3 轮采集源失败置位，岛收缩态显示"采集异常"） | store、claude_code、service、IslandBar |
| ⑦ | 🟢 | **打磨**：限流关键词收紧（"额度/频率"须与状态词组合，防普通通知误判 error）；report_slice 静态 SQL 取代 format! 拼列；`lbutton_down` 补 SAFETY 注释；聚合器改适配器注册表（`Vec<Box<dyn AgentAdapter>>`）；滑动动画超时由独立兜底线程改为时间戳自复位；数据保留默认"永不"→"1 年" | state、store、lib.rs、Settings.tsx |

> 自库幂等键口径注记：UNIQUE(agent， session_id， ts， model) 意味着同毫秒同模型的
> 两条不同消息会合并保留最大快照——与 ccusage 同口径，极低频，接受（审查 3.4）。
> 已知未做：clippy 未安装（网络不可达，恢复后 `rustup component add clippy` 补跑）；
> glm_token 明文落库维持现状（与同类工具一致，DPAPI 加密列 M2 可选）。
