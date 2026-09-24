# HANDOFF 交接快照

> 任何 Agent/人接手前必读（顺序：AGENTS.md → WORKFLOW.md → 本文件 → 03-TASKS.md）
> 维护规则：每完成一个任务或结束一次会话，更新本文件

## 当前状态（2026-09-24 更新）

- **阶段**：阶段 3（实施）进行中，M0 已收尾；M1 全部完成（M1-2 搁置等所有者清单）；
  **M2 P0/P1/P2 全部完成＋M2-13 已提交代码（2026-09-24，OpenClaw 适配器，装机对账留待）**——
  源码级调研先行（openclaw/openclaw main 逐文件，详 01-RESEARCH §14，所有者拍板
  本机不装机）**三处总纲外新知**：①每 agentId 一库（`<root>/agents/<id>/agent/`，
  多库枚举＋session_key 跨库同名须带 agentId 消歧，incognito 哨兵保留名排除）；
  ②转录事件 ≥1KB 转 zstd 是常态（event_zstd BLOB＋utf8_bytes 长度校验，Rust 侧
  新增 zstd 0.13 解压）；③回合式一次性落盘（无 CC 式流式中间快照，usage 即每回合
  增量，无双计面）。实施：`collector/openclaw.rs`——状态根四候选（OPENCLAW_STATE_DIR
  校验存在＞~/.openclaw＞~/.openclaw-<profile>＞~/.clawdbot 旧目录）＋scan
  （session_nodes 90 天，display_name>label 标题链，窗口 join 取模型列）＋collect
  （窗口 updated_at>水位-60s 选活跃窗＋per-window seq 水位只解析新增，重启全窗
  重放靠 `oc:{agent}:{key}:{seq}` 幂等键兜底，官方 assistantIdempotencyKey 优先
  `ocm:{key}`）＋usage 四桶直取（input 落盘前已归一不含缓存，免拆分）＋
  stopReason=error→错误行（弱信号）/aborted→用户打断不计（对齐 ZCode cancelled）＋
  DirScan 快轮＋进程匹配声明化；前端 AGENT_DEFS +1（openclaw 龙虾红 #ef4444）。
  验证：cargo test **78/78** 全绿（新增 7）＋npm build 通过；
  `test_real_openclaw` 标 #[ignore] **装机后补跑**（清单 01-RESEARCH §14.3 八项）。
  ⏳ 待所有者验收后提交。
  **M2-12 OtelSink 骨架代码完成（2026-09-23，待所有者验收）**——outfile 字段级源码
  调研先行（详 01-RESEARCH §13.4）新确证五点：**Gemini api_response 双记录须按
  event.name 过滤防双计**、Qwen 单记录顶层展开（无 tool 有 response_id/ttft_ms）、
  outfile JSON 仅 attributes 可枚举（时间取 event.timestamp）、启用需 enabled+outfile
  双前提、api_error 两家都有（比转录 error 文本精确）。实施：collector/otel.rs
  公共模块（OtelProfile 差异声明＋发现链 JSONC/env/mtime 缓存＋pretty JSON 值流
  StreamDeserializer 游标——截断停值起点/UTF-8 残缺只解析合法前缀/中段坏数据从
  失败点跳行防卡死）＋两家适配器组合挂载（无新引擎管线，实施裁剪回写总纲 M2-12 行）；
  **通道裁定（所有者拍板）：token 行解析与对账就绪但暂不入库**（与转录通道同回合
  无公共 id 可对齐，入库必双计；装机对账后可一行切换），**api_error 行入库走
  recent_error**（转录/hooks 均无的精确信号）；隐私白名单取数（logPrompts 脏字段
  不碰）。验证：cargo test **71/71** 全绿（新增 10）＋check 零告警；
  `test_real_otel_gemini`/`test_real_otel_qwen` 标 #[ignore] **装机后补跑**
  （清单 01-RESEARCH §13.3 增补 9~12：路径形态/session.id 同源/首读性能/env 名）。
  **所有者 dev 验收通过（2026-09-23），随本会话提交。**
  **M2-11 已提交（2026-09-23，Gemini CLI＋Qwen Code 双适配器＋hooks 注入，所有者验收通过）**——
  两轮源码级调研（数据面＋hooks 字段级，详 01-RESEARCH §13）**推翻总纲三处预想**：
  ①「Qwen 同族参数化换根即用」不成立（fork 基线 v0.8.2 自 v0.1 起停止同步，落盘
  已大幅分叉）→ **两份独立适配器**（gemini.rs/qwen.rs，仅共享 cached ⊆ prompt
  拆分口径）；②Gemini「无 hooks」不成立（11 事件 CC 式）＋Qwen 22 事件几乎全 CC
  同名 → 所有者拍板扩大范围，**hooks 注入一并做**；③Gemini chats 已 JSONL 化
  （同 id 消息重 append＋$set/$rewindTo 控制行，slug 目录）。
  实施：Gemini=tmp/*/chats/session-*.jsonl tail＋metadata 头缓存（sessionId 永不变）
  ＋`gm:{id}` 幂等（重 append 靠库层 UPSERT 后写覆盖）＋$set 只取 summary＋error 行
  走 recent_error＋GEMINI_CLI_HOME 重定向＋注入 7 事件（无 async、timeout 毫秒 5000）；
  Qwen=projects/*/chats/*.jsonl 纯追加 tail＋文件名即 uuid＋行内 cwd 直取＋
  `qw:{uuid}` 幂等＋custom_title 宽容提取＋QWEN_HOME/QWEN_RUNTIME_DIR 双重定向＋
  注入 10 事件（timeout 秒+async:true+shell powershell，零映射改动全直通状态机）；
  hooks JSON 注入第三次重复即抽象——CC 注入核心迁 engine.rs 公共化
  （inject_json_hooks/uninstall_json_hooks 三家共用）；状态机映射表＋4 个 Gemini
  差异事件；前端 AGENT_DEFS +2（gemini #4285f4/qwen-code #a78bfa）、HOOKS_AGENTS 3→5。
  验证：cargo test **61/61** 全绿（新增 6）＋check 零告警＋npm build 通过；
  `test_real_gemini`/`test_real_qwen` 标 #[ignore] **装机后补跑**
  （清单 01-RESEARCH §13.3：落盘核对/拆分口径对账/custom_title 载体字段/
  runtime.json/hooks 触发链/OTel outfile 样本；P1/P2 四家装机清单一并待办）。
  **所有者 dev 验收通过（2026-09-23），随本会话提交。**
  **M2-10a 已提交（2026-09-23，OpenCode＋MiMo Code 同族适配器，所有者验收通过）**——
  源码级调研先行（所有者拍板不装机，详 01-RESEARCH §12）**推翻总纲预想**：
  OpenCode 主存储已迁 SQLite（v1.18.32 `~\.local\share\opencode\opencode.db`，
  JSON storage 仅剩边缘用途）；MiMo 为 OpenCode fork 确证（`MIMOCODE_HOME` 重定向，
  `~\.local\share\mimocode\mimocode.db`）。通道优先级反转：**只读 SQLite 为主通道，
  SSE（`/event`，session.next.step.ended 带 usage）降为 opt-in 增强档=M2-10b
  （装机后实施，端口发现/事件流实测为前提）**。
  实施：`collector/opencode.rs` 新建 `OpenCodeFamilyAdapter` 同族参数化（两家一 struct，
  差异仅 FamilyProfile 常量：agent id/数据根/db 文件名/重定向变量/进程关键词，SQL 与
  data JSON 解析零复制粘贴）；session 表 scan + message 表水位增量（**按 time_updated
  而非 created**——流式行原地更新可重采，`oc:`/`mc:msg_{id}` 幂等）+ error 行走
  recent_error 链路 + **-wal 文件快轮信号**（WAL 主库 mtime 不动）；前端 AGENT_DEFS
  +2 行（opencode 青色/mimo-code 橙色）；无 hooks（两家无 CC 式体系）、自库零迁移。
  验证：cargo test 55/55 全绿（新增 7 单测：合成样本提取/水位增量/幂等/MIMOCODE_HOME
  回落/xdg 两分支/截断防多字节 panic）、npm build 通过；
  `test_real_opencode`/`test_real_mimo` 已写好标 #[ignore] **装机后补跑**
  （清单 01-RESEARCH §12.3：落盘路径/真实样本对账/聚合列交叉验证/端口发现/进程名；
  P1 两家 Codex/Kimi 的装机清单 §11.3 一并待办）。
  **待所有者 dev 验收**。
  **M2-UX-2 设置页 Agent 监控卡片网格改版（2026-09-23 所有者插单，验收通过）**——
  删 Claude Desktop 选项＋implemented 死代码；六家改双列卡片网格（列数纯宽度
  自适应 1~4 列封顶，实测 1000→3 列/760→2 列/520→1 列无截断）：checkbox→Switch、
  左缘身份色带＋自绘色点取色、副标题=真实采集方式文案、整卡可点＋事件隔离；
  深浅双主题四档宽度截图自查通过，详 03-TASKS M2-UX-2。
  M2-5（notify 评估）按量化判据观察一周后定。
  **M2-UX-1 面板历史区可见性治理（2026-09-23 代码完成待 dev 验收）**——
  所有者报障「面板漏了 CC 会话」：后端扫描正常（42 ZC＋45 CC=87），根因是
  展示层（idle/offline 状态权重把 CC 钉尾＋截断 20 条）。修复：历史区纯时间
  降序＋Agent 筛选器（纯 hover 交互、仅展开态可见、收起自动清筛选）＋「查看
  更多」带 Agent 预筛选跳会话窗口＋GlobWalker 瞬态失败不缓存空结果。
  单测 41/41、前端构建通过；**2026-09-23 所有者 dev 验收通过**（含交互升级：
  纯 hover 筛选 + 仅展开态可见 + 收起清筛选）；详见 03-TASKS M2-UX-1。
  **数据准确性治理（2026-09-21，M1-12 代码完成待所有者 dev 验收）**——
  全链路核查＋本机对账实锤两个误差源：①CC 流式复制快照行跨采集轮次双计
  （活跃日实测 +27.8%）；②cost-state 后台调用用量未采（缺口 4.55%）。
  修复：迁移 0003 幂等键升级 `(agent,session_id,source_id)`＋存量自动清空重建
  （迁移写 usage_rebuild_pending 标志→聚合器归零水位→首轮全量回溯，重建零损失）；
  cost-state 差值行补采（is_background=1，token 计入、次数不计）；水位钳制 min(now)
  防时钟回拨停更；cancelled_by_user 不再计错误；会话中心冻结三态加时间窗；
  CC 会话标题补采（ai-title 行后写覆盖取最新，修复"标题显示目录名"）。
  **0004 落地修复（2026-09-21 二次发现）**：0003 首版 SQL 的 CREATE IF NOT EXISTS 在
  所有者库上被静默跳过但版本号已消耗到 3——表停留旧 13 列结构、修复全部未生效。
  0004 无条件重建收敛任何中间状态；first_cwd 8KB→64KB（实测覆盖 19/43→43/43）。
  ⚠️ **验收步骤**：托盘退出当前实例 → 重启 dev → 0004 自动重建＋全量回溯（约 10 秒）。
  **端到端对账：重建后今日 CC=696,302 与真实口径分毫不差**（修复前 889,947）。
  验证：cargo test 33/33、ignored 集成通过、check 零告警、npm build 通过。
  详单见 03-TASKS M1-12、01-RESEARCH §8.1。
  **会话中心（2026-09-20，M1-10 ✅ 所有者 dev 验收通过）**——
  所有者拍板"报表页移除会话明细＋新增独立会话窗口"：#sessions 第五辅助窗口
  （分页每页可选 20/50/100＋跨页自增序号列、状态档 chips、关键字搜索、四键排序、
  范围默认今日、CSV 所见即所得
  导出、行点击跳转仅限 30 分钟内活跃会话、数据截至时间戳＋手动刷新）；
  岛面板收口（活跃区 12/历史展开 20 上限，「查看更多会话」＋标题行直达会话窗口）；
  报表页瘦身回归纯聚合分析。口径同源改造：displayState/cardTitle 抽至
  shared/sessionDisplay（与 Rust ENDED_AFTER_MS 双侧同步），SearchSelect 抽至
  shared（防 echarts 漏进会话 chunk）。验证：cargo test 30/30、npm build 通过。
  待议区新增：零用量会话不显示（INNER JOIN 边界）、报表页不补汇总导出。
  详单见 03-TASKS M1-10。
  **会话数据深利用（2026-09-20，M1-11 ✅ 所有者 dev 验收通过）**——
  档一：session_page 补思考 token/平均首字两列（CSV 同步），Token/次数/出错/时长
  悬浮升级（思考占比、平均单次、首末相隔、错误率）；档二：点击行开右侧详情抽屉——
  调用流水（usage_records 倒序≤500）＋状态时间线（status_events 双口径≤200），
  跳转入口迁入抽屉；fmtMs 抽至 shared/format。验证：cargo test 30/30、npm build 通过。
  详单见 03-TASKS M1-11。
  ⚠️ 同期所有者并行重构 lib.rs 岛显隐（peek_apply → island_transition），
  M1-10 改动已在该结构上验证通过；接手者注意两批改动同文件并存。
  **项目定名同步（2026-09-18，所有者确认后实施）**——中文名统一为「去你的岛」；
  同步范围：tauri.conf 四窗口标题
  （岛窗"去你的岛"，辅助窗"设置/报表/关于 · 去你的岛"）、index.html、托盘 tooltip
  （"去你的岛 · AgentTrackerIsland"）、启动文案（"「去你的岛」启动中…"）、
  关于页品牌区与简介、README、AGENTS.md；docs/02-DESIGN、03-TASKS
  记录同步更新。纯展示文案，productName/identifier/仓库名未动。
  **边角细节优化批次（2026-09-18，M1-8，代码完成待所有者 dev 验收）**——
  ①托盘"显示/隐藏灵动岛"与贴边隐藏态互通（恢复走 peek_apply 滑回停靠位，
  原先窗口级 show 只回屏外隐藏位语义失效）；②胶囊两处额度 tooltip 字面 `\n` 修复；
  ③托盘菜单：退出前分隔线、"灵动岛"项文案动态化、tooltip 更名"去你的岛（AgentTrackerIsland）"；
  ④移除未用依赖 window-vibrancy（tauri 自身传递依赖 0.6.0 不受影响）、删 react.svg、
  修 saved_pos 陈旧注释；⑤定名对齐：启动文案/窗口标题（去你的岛 · 设置/报表/关于）/index.html/README
  （纯展示文案，productName/identifier 未动）。托盘图标尺寸暂不处理（所有者拍板）。
  验证：cargo test 29/29 ✅、cargo check 零告警 ✅、npm run build ✅。详单见 03-TASKS M1-8。
  **首验翻车与修复（2026-09-18，两轮定位）**：贴边隐藏态点托盘"显示灵动岛"→
  贴边区变黑、托盘菜单不再弹出、进程假死。真因：peek_apply/apply_snap 尾部持有
  motion 锁（MutexGuard 活到函数末尾）调用 refresh_toggle_text，其内部再次 lock
  motion——非重入互斥量同线程自锁，motion 永久被持有，主线程 Moved 处理器拿锁
  阻塞（悬停滑入同样中招）。修复：先取值放锁再调 refresh。第一轮误判为菜单回调
  上下文与合成管线互等（转线程修复无效但保留作卫生习惯）。
  **托盘显隐语义重构（2026-09-18 二轮，所有者确认后实施，待 dev 验收）**：
  菜单项去"…"；显隐项=纯窗口可见性判定的"显示/隐藏"——"隐藏"=任意可见态
  （胶囊/贴边标签）一键整窗消失（游戏清屏场景）；"显示"=临时召唤，先亮胶囊
  提醒位置，停靠边缘且自动隐藏开启则 3s 自动滑出收回（island-summon 事件→
  前端定时器，鼠标移入取消）。验证：cargo test 29/29 ✅、check 零告警 ✅、
  npm build ✅。待验收清单见 03-TASKS M1-8。
  注意：工作区另有一处所有者未提交改动（Panel.tsx 模型 tooltip 文案微调），未纳入本批次。
  **第六次会话：设置页设计与交互改造（2026-09-17，所有者验收通过）**——
  ①布局：七节并五节（通用/灵动岛/Agent 监控/额度与凭据/数据与维护），
  分区头主副标题同行不换行，统一"设置行"=固定标题+固定描述+右侧控件，
  文案不随选中态变化（遵循 Fluent 开关文案规范）；
  ②交互：设置项全部即时生效（去保存按钮、去保存后自动关窗），主题三档分段控件
  点击即全窗口预览，CSS 滑动开关，API Key 明文切换+来源徽标，恢复默认颜色；
  ③反馈：校验错误内联就近显示（阈值/撞色），操作结果顶部 toast（成功 2.5s 自灭/
  失败常驻）；④窗口 800×600（4:3 横向）可调，min 520×480（按最宽副标题实测 395px
  +缓冲）；⑤文案全中文标点；统计保留时长 7 档（12 个月默认～1 天，旧档位存储值
  回落默认）；⑥色板新增 --accent/--danger/--switch-off 分主题定义。
  验证：npm run build ✅ + 视觉门禁（深浅双主题/交互态截图逐条核对）通过；零 Rust 改动。
  详单见 02-DESIGN §6。**定名：中文名「去你的岛」，定位口号「主流 Agent 状态实时监控
  灵动岛工作台」，README/宪法/仓库 About 已同步（应用内 UI 未改名，productName
  涉及 %APPDATA% 数据目录，改名需迁移方案后再议）。**
  **第五次会话：全面代码审查优化已落地（2026-09-17，代码+文档回写完成，
  已随第六次会话一同提交；Rust 侧运行时行为仍待所有者实机验收）**——
  按"架构/框架/数据安全/性能"四维审查报告逐项实施，
  详单见 02-DESIGN §8。要点：①可观测性（新增 logging.rs 文件日志+panic 钩子+
  聚合线程 catch_unwind+快照 degraded 标志；release 去 panic=abort）；
  ②hook 事件文件 seek 增量读+修复重建后失明 bug+8MB 轮转；③CC 转录 per-file
  偏移增量读+cwd 缓存；④安全（CSP 启用/get_settings 不再下发 glm_token/
  set_setting 白名单/移除 opener 依赖与多余权限/hooks rename 重试+备份留 5 份）；
  ⑤性能（报表 async+spawn_blocking/批量查询+迁移 0002 索引/sysinfo 复用 30s/
  GLM Client 复用）；⑥数据保留默认改"1 年"（第六次会话改为 12 个月～1 天 7 档）。
  **验证：cargo test 23/23 ✅、
  npm run build ✅、cargo check 零告警**；新增回归测试 4 个（事件重建归零/
  轮转/半行恢复/CC 增量）。验收注意：岛收缩态文案新增"· 采集异常"降级提示
  （仅连续 3 轮采集失败时出现）；限流关键词收紧后普通"额度"通知不再标红。
- 环境备注：clippy 未安装（会话内外网不可达），网络恢复后
  `rustup component add clippy` 补跑；日志开关 `AT_LOG=debug`（默认 info），
  日志文件 %APPDATA%\com.agenttrackerisland.app\logs\agenttrackerisland.log
- **第四次会话（2026-09-17）：M1-4 深浅双主题**——纯前端 8 处改动、零 Rust 零权限：
  ①新增 shared/theme.ts（ThemeMode 三态存 app_settings `theme` 键 + useTheme Hook:
  启动读设置写 `<html>` data-theme / 监听设置页 theme-changed 广播 / system 模式订阅
  matchMedia）；②App.css 色板变量化（：root 暗色默认=历史值不闪变 +
  [data-theme="light"] 覆盖，23 语义变量），settings.css/report.css 改引变量；
  ③设置页"外观"区块，保存后三窗口即时切换；④报表 ECharts 轴/网格/饼图标签随主题。
  跟随系统信号链已核对本地 wry/tauri-runtime-wry 源码（系统切换→ThemeChanged→
  SetPreferredColorScheme→matchMedia 触发）。验证：npm run build ✅、cargo test 19/19 ✅、
  默认深色胶囊实屏确认 ✅；**浅色/跟随系统/报表图表待所有者验收**。
  已知限制：settings/report 原生标题栏颜色跟随系统不随主题选择（内容区跟随，后续按需）
- 会话插曲（不影响交付）：Agent 桌面自验时键盘事件误触托盘菜单导致 dev 实例两次
  干净退出（quit 是唯一 exit（0） 路径，非代码缺陷）；所有者指示"验收由本人操作电脑"，
  Agent 已停止桌面控制并清理残留进程/1420 端口
- git：M1-1/M1-6/M1-7 代码与文档回写已提交推送（aa7a0b3,2026-09-17）；
  **2026-09-17 项目更名 AgentTrackerIsland，全量标识更正随本次会话提交**；
  **M1-4 双主题已提交推送（7b5bad3,2026-09-17,10 文件 +332/-103）**
- 注意（2026-09-17 更名）：项目由 AgentTracker 更名为 **AgentTrackerIsland**，实际路径
  F:\MyProjectRepository\AgentTrackerIsland（旧文档中 F:\MyProjectRepository\AgentTracker /
  F:\AgentTracker 均为更名前写法）；标识符同步切换：GitHub 仓库 ldtmore/AgentTrackerIsland、
  npm/cargo 包名 agenttrackerisland、lib 名 agenttracker_island_lib、Tauri identifier
  com.agenttrackerisland.app、自库 %APPDATA%\com.agenttrackerisland.app\agenttrackerisland.db、
  事件目录 %LOCALAPPDATA%\AgentTrackerIsland\events——旧路径数据留在原地未迁移，
  更名后需在设置页**重新安装 hooks**（旧 hook-bridge 仍写旧事件目录）；
  调研存档 E:\AIAgentTemp\AgentTracker-research\ 为更名前目录，保留原名

## 下一步

1. **M2 推进中**（总纲 04-EXPANSION，P0/P1/P2 已全部完成）：
   **最新＝M2-13 OpenClaw 适配器代码完成（2026-09-24，待所有者验收后提交）**，
   装机对账 `test_real_openclaw` 留 §14.3 清单；M2-5 notify 评估按量化判据
   观察一周后回写结论（P0 上线日 2026-09-23 起算）
2. **装机驱动批次**（所有者装机后集中补跑，清单已备）：`test_real_*` 共 9 项
   （Codex/Kimi/OpenCode/MiMo/Gemini/Qwen/OTel×2/OpenClaw，见 01-RESEARCH
   §11.3/§12.3/§13.3/§14.3）＋hooks 实机触发链＋M2-10b SSE 增强档
3. **P3 三家**（下一批开发任务，照 M2-13 模式源码调研先行）：M2-14 Hermes →
   M2-15 Copilot CLI（SQLite 档，OpenClaw 多库模式可复用）；M2-16 WorkBuddy
   勘察定档；逆向档 Cursor/Windsurf 押后
4. 穿插项：T10 WT 跳转调试（待议区有线索）、CC 未装 hooks 的 mtime 假 working
   噪声（M1-12 遗留观察）；T13 Dogfood 周依赖 T12；**构建打包仅当所有者明确
   宣布"正式对外发布"时执行**（WORKFLOW 构建打包纪律）
环境提示：cargo 带 RUSTUP_HOME/CARGO_HOME/PATH，外网走本机代理 127.0.0.1:6478，
Bash 显式 cd 到项目目录；**dev 验收提示：Agent 会话曾出现托盘菜单误触（键盘事件），
人工操作无此风险；启动前确认 agenttrackerisland.exe 无残留、1420 端口空闲**。
环境提醒：cargo 带 RUSTUP_HOME/CARGO_HOME/PATH，外网走本机代理 127.0.0.1:6478，
Bash 显式 cd 到项目目录；**dev 验收提示：Agent 会话曾出现托盘菜单误触（键盘事件），
人工操作无此风险；启动前确认 agenttrackerisland.exe 无残留、1420 端口空闲**。

## 关键背景（新接手者必读）

1. 本工具是**旁路观测台**，五条红线见 AGENTS.md，任何实现决策不得违反
2. ZCode 适配器 = 只读 `~\.zcode\cli\db\db.sqlite` 的 `model_usage` 表（勘察结论见 01-RESEARCH §1）
3. Claude Code 适配器 = 解析 `~\.claude\projects\**\*.jsonl` + hooks 事件文件（协议 02-DESIGN §4）
4. GLM 额度 = Monitor API（`/api/monitor/usage/quota/limit`，裸 key Authorization），响应字段见 01-RESEARCH §7
5. hooks 官方文档站本机不可达 → T6 第一步先装诊断 hook 实测 stdin 字段
6. 调研原始材料（竞品 README/GLM 源码/官方文档摘录）在 `E:\AIAgentTemp\AgentTracker-research\`（2026-09-17 项目更名前存档，路径保留原名）

## 踩坑记录

- **MutexGuard 持锁自锁坑（2026-09-18，M1-8，两轮定位）**：`let m = mutex.lock()` 的
  守卫实现 Drop，**活到函数末尾而非最后使用处**——函数尾部持锁调用了会再次 lock
  同一互斥量的函数（refresh_toggle_text），std::sync::Mutex 非重入 → 同线程自锁。
  症状极具迷惑性：卡死的是"当前线程+所有后续拿锁线程"，主线程在 Moved 事件处理器
  拿锁处冻结 → 透明窗口变黑+托盘菜单弹不出+进程假死，而后台无关线程（聚合器）
  一切正常，极易误判为"上下文/合成管线互等"（第一轮就误判了，转线程修复无效）。
  排障抓手：日志走到哪一步停了 + 哪些线程还活着。规矩：**锁内绝不调用会再拿
  同一把锁的函数；取值放锁再调下游**（lib.rs peek_apply/apply_snap 尾部注释）
- **DPI 逻辑/物理坐标坑（M1-6）**：窗口尺寸（tauri.conf）是逻辑像素，事件/显示器坐标是
  物理像素，缩放 ≠100% 时直接混用会导致贴边判定偏移、隐藏标签"飘"到屏中间——
  贴边几何全链路统一逻辑坐标（phys_to_logical/monitor_logical 换算，落位再转回物理）；
  另：程序化 set_position 产生的 Moved 事件与用户拖拽不可区分，必须走动画标记+落点
  消费+滑动代数三重防护，启动定位也要走同一通道
- **drag-region 权限坑（M1-6 实测）**：`data-tauri-drag-region` 底层走 `start_dragging`
  命令，而 `core:window:default` 权限集**不含** `allow-start-dragging`（只有只读类）——
  权限被拒时前端静默无反应，T8 起拖拽从未生效直到 M1-6 验收才暴露。修复：capabilities
  加 `core:window:allow-start-dragging`。教训：涉及交互的新能力，验收必须真实操作一遍
- **程序化移动 vs 拖拽事件坑（M1-6）**：代码里 `set_position` 会触发 Moved 事件，与用户
  拖拽不可区分——贴边吸附用"动画标记 animating + 落点 programmed + 滑动代数 SLIDE_GEN"
  三重机制屏蔽自身事件，启动定位也必须走同一通道，否则会自触发吸附循环
- **项目迁移目录坑（2026-09-16 验证实测）**：target/ 构建缓存嵌旧绝对路径（F:\AgentTracker），
  目录变更后 tauri 构建脚本报"系统找不到指定的路径"（指向旧盘符路径）——`cargo clean`
  全量重建即可恢复（约 6 分钟）；2026-09-17 更名迁移到 F:\MyProjectRepository\AgentTrackerIsland
  后同样执行了 cargo clean 重建
- **UI 三连坑（T8 实战）**：①写前端文件路径勿多一层（曾误写 src/src/App.tsx 导致 vite 一直服务模板——Write 成功≠路径正确，**UI 改动必须以屏幕真实渲染为验收**）；②window-vibrancy/Acrylic 是**窗口级**效果，整个矩形窗口变磨砂灰，胶囊形态必须"窗口全透明+CSS 自绘背景"（依赖保留未用）；③tauri dev 用 TaskStop 后 agenttrackerisland.exe 与 vite 可能残留并占 1420 端口，重启 dev 前先 taskkill + 清端口
- **Claude Code JSONL 数据知识（T4 实测）**：同一 assistant 消息平均重复 ~3 次（流式快照/会话恢复复制），必须按 messageId（+requestId）去重并保留用量最大快照；`<synthetic>` 行是本地合成消息（usage 全 0）须过滤；`cost-state` 行是会话级累计快照（ccusage 纳入、我们没有，对账差 1.7% 的来源）；本机数据无 requestId 字段；**集成测试勿断言"增量采集为空"**（活跃会话在写入，竞态必挂，容忍 ≤5 行）——对账详情见 01-RESEARCH §8
- **参考工具优先**：遇到解析/口径问题先看调研存档 E:\AIAgentTemp\AgentTracker-research\（better-ccusage/ccusage/glm-quota-line 源码），别自己盲试变体
- **Rust 测试要点**：mod tests 所在文件必须在父 mod.rs 里声明（`pub mod zcode;`），否则整文件不参与编译且无任何警告；模型名比较一律小写化（真实数据 GLM-5.3/glm-5.3 混用）；rusqlite 0.40 的 Error 无 io 变体，统一用 anyhow
- **🌐 所有外网访问（rustup/crates.io/npm/GitHub 下载）走所有者本机代理 `127.0.0.1:6478`**（2026-09-16 所有者指示，速度关键）：curl 用 `-x 127.0.0.1:6478`，或设 `HTTPS_PROXY/HTTP_PROXY`
- **安装顺序坑**：rustup-init 检测不到 MSVC 时会自动往 C 盘装 VS Build Tools——必须先装 VS（D:\VSBuildTools）再跑 rustup-init
- Anthropic 文档站（code.claude.com/docs.anthropic.com）直连与 Jina 代理均超时 → hooks 细节走实测
- gh search 的 JSON 字段名用 `language`（不是 primaryLanguage）；`gh repo view` 用 `stargazerCount`
- 本机 Node 24 内置 `node:sqlite`，读 ZCode 库用它即可（readOnly 打开，WAL 并发读安全）
- 所有者 D 盘为"软件安装盘"：每软件一目录平铺（D:\Git、D:\Python314…）；本项目工具链布局 D:\Rust\{rustup,cargo} + D:\VSBuildTools（2026-09-16 确认）

## 所有者偏好（交互层面）

- 简体中文交流；结构化表格+emoji；先计划后编码；重大变更先确认
- 修改>3 文件或>10 行代码先列计划（本看板任务已视同获批计划，但看板外变更仍需确认）
- 不自动 git commit，等确认
- **构建打包是最后一步**：仅当所有者明确提出"打包构建正式对外发布"时才执行
  （release/zip/安装包）；其余阶段一律本机调试开发 + GitHub 代码提交，任何 Agent
  不得主动 `tauri build`。cargo test/npm run build 等验证性构建不受限。
  （2026-09-17 指示，详见 WORKFLOW 构建打包纪律）
