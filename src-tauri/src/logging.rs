//! 文件日志（2026-09-17 日志系统升级）：
//! `log` 门面（已是 tauri 传递依赖，零新增外部依赖）+ std 纯实现。
//! - 按天分文件：`{前缀}_YYYYMMDD.log`，写入中的文件始终保持当日固定名
//! - 单文件超限滚动：改名为当日下一档 `{前缀}_YYYYMMDD_N.log`（N = 当日最大序号 + 1）
//! - 保留期限 + 总量兜底：启动 / 跨天首条 / 每次滚动时惰性清理，无常驻线程与定时器
//! - 开发者模式：`set_verbose` 即时切换 Debug/Info；业务埋点（target 前缀 `metric::`）
//!   预留挂 Debug 之下，模式关闭时被 max_level 短路，零开销
//!
//! 性能与可靠性取舍：
//! - 每条日志 open+write+close，不长持句柄：Windows 下 rename 正被写入的文件会失败，
//!   不持句柄则滚动/清理零冲突；日志低频（每 10 秒数条）时开销微秒级，无感
//! - 文件大小用内存累计（启动时按真实大小校准一次），省掉每条 stat 系统调用
//! - 滚动/清理失败（Windows 杀软短时锁定文件是常态）一律静默、下条日志重试；
//!   日志实现内绝不允许 panic，append 失败丢弃该条即可
//! - 动机（继承自 2026-09-17 审查优化 1.1）：采集/额度/写库失败此前被静默吞掉，
//!   "静默降级"变成了"静默失明"——所有降级路径从此必须留痕。
//!
//! 用法：setup 里 `logging::init(&app_data_dir.join("logs"))` 一次；其余代码直接 `log::warn!/info!`。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{Local, NaiveDate};

/// 日志配置：集中定义，需求变更只改这里（init 时固化，不做用户级设置——
/// 属维护者参数而非用户偏好，且 init 在 setup 极早期执行，Store 尚未就绪）
#[derive(Clone, Copy)]
pub struct LogConfig {
    /// 日志文件名前缀（文件名规则：{前缀}_YYYYMMDD.log，滚动档加 _N）
    pub file_prefix: &'static str,
    /// 单文件大小上限（字节），超过滚动出下一档
    pub max_file_bytes: u64,
    /// 日志保留天数，文件名日期早于（今天 − N 天）即清理
    pub retention_days: u32,
    /// 日志目录总量兜底上限（字节），超限从最老的文件删起
    pub max_total_bytes: u64,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            file_prefix: "atilog",
            max_file_bytes: 20 * 1024 * 1024,
            retention_days: 30,
            max_total_bytes: 200 * 1024 * 1024,
        }
    }
}

/// 运行状态：当前写入日、当前文件累计字节数、上次清理日（Mutex 保护，见 log() 注释）
struct State {
    day: NaiveDate,
    written: u64,
    last_cleanup: NaiveDate,
}

struct FileLogger {
    /// 日志配置（init 时固化）
    cfg: LogConfig,
    /// 日志目录
    log_dir: PathBuf,
    /// 运行状态（log 门面要求 Sync，并发日志调用经此串行化；单锁粒度，低频下无竞争）
    state: Mutex<State>,
}

impl FileLogger {
    /// 当前写入目标文件路径：{前缀}_YYYYMMDD.log（当日固定名）
    fn current_path(&self, day: NaiveDate) -> PathBuf {
        self.log_dir
            .join(format!("{}_{}.log", self.cfg.file_prefix, day.format("%Y%m%d")))
    }
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        log::max_level() >= metadata.level()
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // 时间戳用本机时区（chrono 已是依赖），一次取 now 同时供时间戳与日期用
        let now = Local::now();
        let line = format!(
            "[{}] [{}] [{}] {}\n",
            now.format("%Y-%m-%d %H:%M:%S%.3f"),
            record.level(),
            record.target(),
            record.args()
        );
        // 写入路径全程互斥：跨天判定、滚动、计数更新串行化，避免并发 rename 竞争
        if let Ok(mut st) = self.state.lock() {
            let today = now.date_naive();
            if today != st.day {
                // 跨天：切到新日期文件从零计数；新的一天先做一次定期清理
                st.day = today;
                st.written = 0;
                if st.last_cleanup != today {
                    st.last_cleanup = today;
                    cleanup_dir(&self.log_dir, &self.cfg, today);
                }
            }
            if st.written >= self.cfg.max_file_bytes {
                // 超限滚动；rename 失败则 written 保持超限，下条日志再试（尽快恢复上限语义）
                if rotate_file(&self.log_dir, &self.cfg, st.day) {
                    st.written = 0;
                }
            }
            // 追加并累计字节数（内存计数替代每条 stat；20 MB 上限误差几 KB 无影响）
            if let Ok(mut f) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.current_path(st.day))
            {
                if f.write_all(line.as_bytes()).is_ok() {
                    st.written += line.len() as u64;
                }
            }
        }
    }

    fn flush(&self) {}
}

/// 解析日志文件名 →（日期，档序）：固定名 `{前缀}_YYYYMMDD.log` 档序为 0，
/// 滚动档 `{前缀}_YYYYMMDD_N.log` 档序为 N；前缀不符或格式不对的文件返回 None（一律不碰）
fn parse_log_name(name: &str, prefix: &str) -> Option<(NaiveDate, u32)> {
    let stem = name.strip_suffix(".log")?;
    let rest = stem.strip_prefix(prefix)?.strip_prefix('_')?;
    let (day_str, seq) = match rest.split_once('_') {
        Some((d, s)) => (d, s.parse::<u32>().ok()?),
        None => (rest, 0),
    };
    Some((NaiveDate::parse_from_str(day_str, "%Y%m%d").ok()?, seq))
}

/// 扫目录返回当日滚动档最大序号（无则 0）；目录项少（30 天 × 每天数档），毫秒级
fn scan_max_seq(dir: &Path, prefix: &str, day: NaiveDate) -> u32 {
    let mut max_n = 0u32;
    if let Ok(rd) = fs::read_dir(dir) {
        for (d, seq) in rd
            .flatten()
            .filter_map(|e| parse_log_name(&e.file_name().to_string_lossy(), prefix))
        {
            if d == day && seq > max_n {
                max_n = seq;
            }
        }
    }
    max_n
}

/// 滚动：当前文件改名进当日下一档（序号 = 当日最大 + 1，扫描得出，重启后依然正确），
/// 原固定名文件此后从零重写。rename 失败返回 false，由调用方保持超限状态、下条日志重试；
/// 成功后顺带做一次总量兜底清理（约每 20 MB 一次，毫秒级目录扫描）
fn rotate_file(dir: &Path, cfg: &LogConfig, day: NaiveDate) -> bool {
    let next = scan_max_seq(dir, &cfg.file_prefix, day) + 1;
    let from = dir.join(format!("{}_{}.log", cfg.file_prefix, day.format("%Y%m%d")));
    let to = dir.join(format!(
        "{}_{}_{next}.log",
        cfg.file_prefix,
        day.format("%Y%m%d")
    ));
    if fs::rename(&from, &to).is_err() {
        return false;
    }
    cleanup_dir(dir, cfg, day);
    true
}

/// 清理：删文件名日期超保留期的日志；再按总量兜底从最老删起。
/// 只认本组件命名规则（parse_log_name 过滤），目录里其他文件一律不碰；
/// 当前写入文件日期最新、档序最小，升序遍历时天然最后才轮到
fn cleanup_dir(dir: &Path, cfg: &LogConfig, today: NaiveDate) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    // (日期，档序，字节数，路径)
    let mut files: Vec<(NaiveDate, u32, u64, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            parse_log_name(&e.file_name().to_string_lossy(), &cfg.file_prefix).map(|(d, seq)| {
                (d, seq, e.metadata().map(|m| m.len()).unwrap_or(0), e.path())
            })
        })
        .collect();
    // 保留期限：早于「今天 − retention_days」的删除
    let expire_before = today - chrono::Duration::days(cfg.retention_days as i64);
    files.retain(|(d, _, _, path)| {
        if *d < expire_before {
            let _ = fs::remove_file(path);
            return false;
        }
        true
    });
    // 总量兜底：未超上限则无事；超限按（日期，档序）升序从最老删起
    if files.iter().map(|f| f.2).sum::<u64>() <= cfg.max_total_bytes {
        return;
    }
    files.sort_by_key(|(d, seq, ..)| (*d, *seq));
    let mut total: u64 = files.iter().map(|f| f.2).sum();
    for (_, _, size, path) in &files {
        if total <= cfg.max_total_bytes {
            break;
        }
        if fs::remove_file(path).is_ok() {
            total -= size;
        }
    }
}

/// 环境变量兜底级别：`ATI_LOG=debug/trace` 时 Debug（临时排障手段，不污染用户设置），否则 Info。
/// （ATI = AgentTrackerIsland 首字母缩写）
fn env_level() -> log::LevelFilter {
    match std::env::var("ATI_LOG").as_deref() {
        Ok("debug") | Ok("trace") => log::LevelFilter::Debug,
        _ => log::LevelFilter::Info,
    }
}

/// 切换开发者模式（设置页开关，即时生效免重启）：开 = Debug；关 = 回落环境变量
/// `ATI_LOG`（仍为 debug/trace 时保持 Debug）。切换动作本身写 info 留痕。
pub fn set_verbose(enabled: bool) {
    let level = if enabled {
        log::LevelFilter::Debug
    } else {
        env_level()
    };
    log::set_max_level(level);
    log::info!("开发者模式{}", if enabled { "已开启" } else { "已关闭" });
}

/// 初始化全局日志：按默认 `LogConfig` 建目录、启动清理一次、校准当日文件已写字节。
/// 级别：开发者模式由 setup 恢复设置后经 `set_verbose` 切换；环境变量 `ATI_LOG=debug`
/// 为临时排障兜底。重复调用无害（仅首次生效）。
pub fn init(log_dir: &Path) {
    let _ = fs::create_dir_all(log_dir);
    let cfg = LogConfig::default();
    let today = Local::now().date_naive();
    // 启动清理（此后每天首条日志时再清）
    cleanup_dir(log_dir, &cfg, today);
    // 启动校准：当日文件已写字节，重启后计数从真实大小起步（已超限则首条日志先滚动）
    let cur = log_dir.join(format!("{}_{}.log", cfg.file_prefix, today.format("%Y%m%d")));
    let written = fs::metadata(&cur).map(|m| m.len()).unwrap_or(0);
    // set_boxed_logger 仅可成功一次；失败说明已初始化，忽略
    if log::set_boxed_logger(Box::new(FileLogger {
        cfg,
        log_dir: log_dir.to_path_buf(),
        state: Mutex::new(State {
            day: today,
            written,
            last_cleanup: today,
        }),
    }))
    .is_ok()
    {
        let level = env_level();
        log::set_max_level(level);
        log::info!(
            "日志系统初始化完成（级别 {level}，按天分文件，单文件上限 {} MB，保留 {} 天）",
            cfg.max_file_bytes / 1024 / 1024,
            cfg.retention_days
        );
    }
}

/// 安装全局 panic 钩子：任何线程 panic 先落盘再走原钩子，
/// 与聚合线程的 catch_unwind 配合（审查 1.1：panic 必须留痕）。
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| l.to_string())
            .unwrap_or_else(|| "未知位置".into());
        let msg = payload_str(info.payload());
        log::error!("panic @ {loc}: {msg}");
        prev(info);
    }));
}

/// 提取 panic payload 的可读文本（&str/String/其他）
fn payload_str(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "非字符串 panic".into()
    }
}

/// 供 spawn_aggregator 打印 catch_unwind 捕获的 payload
pub fn panic_payload_str(p: Box<dyn std::any::Any + Send>) -> String {
    payload_str(p.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时目录（按测试名 + 进程号隔离，开头清掉残留）
    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atilog-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// 在目录里造一个日志文件（写 size 字节，内容无所谓）
    fn touch(dir: &Path, name: &str, size: usize) {
        fs::write(dir.join(name), vec![b'x'; size]).unwrap();
    }

    #[test]
    fn 文件名解析_固定名与滚动档() {
        let d = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        assert_eq!(parse_log_name("atilog_20260910.log", "atilog"), Some((d, 0)));
        assert_eq!(parse_log_name("atilog_20260910_1.log", "atilog"), Some((d, 1)));
        assert_eq!(parse_log_name("atilog_20260910_23.log", "atilog"), Some((d, 23)));
        // 前缀不符 / 日期非法 / 非本组件命名：一律不认
        assert_eq!(parse_log_name("agenttrackerisland.log", "atilog"), None);
        assert_eq!(parse_log_name("atilog_bad.log", "atilog"), None);
        assert_eq!(parse_log_name("atilog_20260910.log.old", "atilog"), None);
    }

    #[test]
    fn 序号扫描_取当日最大档() {
        let dir = temp_dir("scan");
        let day = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        touch(&dir, "atilog_20260910.log", 1);
        touch(&dir, "atilog_20260910_1.log", 1);
        touch(&dir, "atilog_20260910_3.log", 1);
        touch(&dir, "atilog_20260909_9.log", 1); // 其他日期不算
        assert_eq!(scan_max_seq(&dir, "atilog", day), 3);
        // 空目录 / 无当日文件：0
        assert_eq!(scan_max_seq(&temp_dir("scan-empty"), "atilog", day), 0);
    }

    #[test]
    fn 滚动_改名为下一档并继续写固定名() {
        let dir = temp_dir("rotate");
        let cfg = LogConfig::default();
        let day = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        touch(&dir, "atilog_20260910.log", 10);
        touch(&dir, "atilog_20260910_1.log", 10);
        touch(&dir, "atilog_20260910_2.log", 10);
        assert!(rotate_file(&dir, &cfg, day));
        // 固定名已让位，新档为最大序号 + 1
        assert!(!dir.join("atilog_20260910.log").exists());
        assert!(dir.join("atilog_20260910_3.log").exists());
    }

    #[test]
    fn 清理_超保留期删除() {
        let dir = temp_dir("retention");
        let cfg = LogConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 9, 17).unwrap();
        touch(&dir, "atilog_20260801.log", 1); // 早于 08-18：删
        touch(&dir, "atilog_20260818.log", 1); // 恰在保留边界（≥ 今天 − 30 天）：留
        touch(&dir, "atilog_20260916.log", 1); // 近期：留
        cleanup_dir(&dir, &cfg, today);
        assert!(!dir.join("atilog_20260801.log").exists());
        assert!(dir.join("atilog_20260818.log").exists());
        assert!(dir.join("atilog_20260916.log").exists());
    }

    #[test]
    fn 清理_总量兜底从最老删起() {
        let dir = temp_dir("total");
        let mut cfg = LogConfig::default();
        cfg.max_total_bytes = 1000;
        let today = NaiveDate::from_ymd_opt(2026, 9, 17).unwrap();
        touch(&dir, "atilog_20260915.log", 600);
        touch(&dir, "atilog_20260916.log", 600);
        touch(&dir, "atilog_20260917.log", 600);
        cleanup_dir(&dir, &cfg, today);
        // 总量 1800 > 1000：删最老两档，直到 ≤ 上限；最新一天（当前写入）保留
        assert!(!dir.join("atilog_20260915.log").exists());
        assert!(!dir.join("atilog_20260916.log").exists());
        assert!(dir.join("atilog_20260917.log").exists());
    }
}
