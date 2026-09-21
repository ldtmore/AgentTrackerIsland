-- AgentTrackerIsland 迁移 0003(2026-09-21,数据准确性治理)
-- ① 幂等键升级为真实消息身份 source_id:旧键 (agent, session_id, ts, model) 识别不了
--   CC 同一 assistant 消息的多条流式复制快照行(usage 全同、timestamp 各异,实测 492/534
--   条消息存在,首末行相差 1~10 秒居多)——工具运行期间这些行跨采集轮次分裂入库造成
--   双计(本机活跃日实测虚高 +27.8%)。新键 (agent, session_id, source_id):
--   CC = message.id(+requestId),ZCode = 源库 model_usage.id,ZCode cost 校准行 = 'cost:' 前缀。
-- ② 新增 is_background 标记:CC cost-state 会话级累计快照中超出 assistant 明细的部分是
--   标题生成等后台调用的真实消耗(本机实测缺口 133 万 token,占 4.55%),以差值行补采——
--   token 计入消耗口径,调用次数不计(is_background=1),避免污染"调用次数/最近模型/调用流水"。
--   SQLite 无法删除表内 UNIQUE 约束,直接重建为空表:usage_records 的两个数据源
--   (CC 转录文件/ZCode 源库)都是完整事实源,清空后由采集层全量回溯自动收敛
--   (所有者已确认"自动清空重建",现存转录最早日期与自库最早一致,重建零损失)。
-- ③ 写入重建标志:聚合器首轮检测后归零水位,全量回溯自然完成重建。

-- 无条件重建：旧库（ver=2）的 usage_records 已存在，CREATE IF NOT EXISTS 会静默跳过
-- 导致新列缺失、后续写入全部失败——必须 DROP 后按新结构建；全新库（ver<1 链式执行）
-- 先建 0001 旧表再在此重建，迁移链保持单调。数据不拷贝：两个数据源
-- （CC 转录/ZCode 源库）都是完整事实源，清空后由采集层全量回溯自动收敛
-- （所有者已确认"自动清空重建"，现存转录最早日期与自库最早一致，重建零损失）。
DROP TABLE IF EXISTS usage_records;

CREATE TABLE usage_records (
  id                       INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id               TEXT NOT NULL,
  agent                    TEXT NOT NULL,
  model                    TEXT NOT NULL,
  provider                 TEXT,
  ts                       INTEGER NOT NULL,
  input_tokens             INTEGER,
  output_tokens            INTEGER,
  reasoning_tokens         INTEGER,
  cache_read_tokens        INTEGER,
  cache_creation_tokens    INTEGER,
  duration_ms              INTEGER,
  ttft_ms                  INTEGER,
  error_type               TEXT,
  -- 真实消息身份:同源多行(流式复制快照)靠它识别,upsert 保留最大快照
  source_id                TEXT,
  -- 后台用量校准行(cost-state 差值):计入 token,不计入调用次数
  is_background            INTEGER NOT NULL DEFAULT 0,
  UNIQUE(agent, session_id, source_id)
);

CREATE INDEX IF NOT EXISTS idx_usage_ts ON usage_records(ts);
CREATE INDEX IF NOT EXISTS idx_usage_session ON usage_records(session_id);

-- 重建标志:聚合器首轮读到后清空水位,采集层全量回溯重建(迁移无法替采集器做)
INSERT OR REPLACE INTO app_settings(key, value) VALUES('usage_rebuild_pending', '1');
