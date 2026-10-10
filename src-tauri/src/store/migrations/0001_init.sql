-- AgentTrackerIsland 自库初始化(0001)
-- 时间戳约定:一律 Unix 毫秒;agent 取值:'claude-code' | 'zcode';provider:'glm' | ...

-- 会话表:一个 Agent 会话一行(id = "{agent}:{sessionId}")
CREATE TABLE IF NOT EXISTS sessions (
  id            TEXT PRIMARY KEY,
  agent         TEXT NOT NULL,
  provider      TEXT,
  model         TEXT,
  project_dir   TEXT,
  title         TEXT,
  first_seen_at INTEGER,
  last_seen_at  INTEGER,
  state         TEXT NOT NULL DEFAULT 'offline',
  state_reason  TEXT
);

-- token 流水:每条模型调用一行;幂等键保证回放补录不重复
CREATE TABLE IF NOT EXISTS usage_records (
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
  UNIQUE(agent, session_id, ts, model)
);
CREATE INDEX IF NOT EXISTS idx_usage_ts ON usage_records(ts);

-- 额度快照:定时拉取保留历史(M1 报表画额度曲线)
CREATE TABLE IF NOT EXISTS quota_snapshots (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  provider     TEXT NOT NULL,
  window_kind  TEXT NOT NULL,           -- '5h' | 'weekly'
  used_percent REAL,
  used_tokens  INTEGER,
  reset_at     INTEGER,
  fetched_at   INTEGER NOT NULL
);

-- hooks/采集原始状态事件(审计与回溯)
CREATE TABLE IF NOT EXISTS status_events (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  agent      TEXT,
  session_id TEXT,
  hook       TEXT,
  payload    TEXT,
  ts         INTEGER NOT NULL
);

-- 各 Agent 采集水位(增量采集的游标)
CREATE TABLE IF NOT EXISTS watermarks (
  agent       TEXT PRIMARY KEY,
  last_ts     INTEGER NOT NULL DEFAULT 0,
  last_offset INTEGER NOT NULL DEFAULT 0
);

-- 应用设置(key-value)
CREATE TABLE IF NOT EXISTS app_settings (
  key   TEXT PRIMARY KEY,
  value TEXT
);
