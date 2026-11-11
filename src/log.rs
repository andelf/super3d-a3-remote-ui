//! 运行日志：把「后处理进程实际拿到什么环境变量」留一份快照。
//!
//! # 为什么要这份日志
//!
//! Orca / Prusa 系切片器调后处理脚本时，除了 argv 里的 G-code 路径，还会把一大票
//! 打印配置通过 `SLIC3R_*` 之类的环境变量塞进来。文档提过这件事，但**具体有哪些、
//! 叫什么、值长什么样，只有真跑一次才知道**。
//!
//! 尤其想知道里面有没有「真实导出文件名 / 输出目录」：有的话，`naming.rs` 那一大套
//! 从 `origin.txt` + `_temp_*.config` 反推名字的逻辑就能换成直接读环境变量，
//! 既更准也不用跟着切片器的内部目录结构走。在拿到实测数据之前，**不改命名逻辑**。
//!
//! # 设计约束
//!
//! - **必须落文件**：脚本的 stderr 被切片器收走，拉起来的 GUI 更是 stdio 全 null；
//!   而且这个进程几百毫秒就退了，来不及看。写文件才能事后查、多次导出能对比。
//! - **绝不影响后处理**：所有 IO 错误一律吞掉，不改退出码、不阻塞、不弹窗。
//! - **只在后处理进程记**：GUI / `--list` / `--status` 启动不写，避免噪声淹没。
//! - **敏感值打码**：环境变量里可能有 token / 密钥，日志是明文落盘的。
//!   确实需要看全值时设 `STARFISH_LOG_FULL_ENV=1`。
//!
//! 日志默认在 `%LOCALAPPDATA%\starfish-control\starfish-control.log`
//! （非 Windows 是 `$TMPDIR/starfish-control/`），可用 `STARFISH_LOG_DIR` 改。
//! 追加写，超过 512 KB 时把旧内容轮转成 `.prev.log`。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::naming;

const LOG_FILE: &str = "starfish-control.log";
const PREV_FILE: &str = "starfish-control.prev.log";
const MAX_LOG_BYTES: u64 = 512 * 1024;
/// 单个环境变量值最多记这么多字符。切片器有些配置是整段 JSON，全记下来会淹掉别的变量。
const MAX_VALUE_CHARS: usize = 4096;

/// 覆盖日志目录。
const DIR_OVERRIDE: &str = "STARFISH_LOG_DIR";
/// 关掉脱敏，把环境变量值原样写进日志（只在排查时开）。
const FULL_ENV: &str = "STARFISH_LOG_FULL_ENV";

/// 日志目录。失败也要能退化——这里返回的路径后面所有 IO 都是 best-effort。
pub fn log_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(DIR_OVERRIDE) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("starfish-control")
}

/// 抓一份后处理进程的环境快照并追加到日志。
///
/// 返回写进去的文件路径；任何一步失败都返回 `None`，调用方不该因此改变行为。
pub fn capture_postprocess(target: &str) -> Option<PathBuf> {
    let full = std::env::var_os(FULL_ENV)
        .map(|v| v.to_string_lossy() == "1")
        .unwrap_or(false);
    let ctx = SnapshotCtx::current(target, full);
    let body = render_snapshot(&collect_vars(), &ctx);
    match append_to(&log_dir(), &body) {
        Ok(path) => {
            // Orca 会收集脚本的 stderr 写进自己的日志，留一行线索方便找文件。
            eprintln!("[starfish] 环境快照已写入 {}", path.display());
            Some(path)
        }
        Err(e) => {
            // 记不上就算了：后处理本身成功更重要，绝不能让切片器以为导出失败。
            eprintln!("[starfish] 环境快照写入失败（不影响后处理）：{e}");
            None
        }
    }
}

// ── 采集 ────────────────────────────────────────────────────────────────

/// 环境变量全量快照。
///
/// Windows 的 `GetEnvironmentStringsW` 里会混进 `=C:` 这种「每个盘符的当前目录」
/// 伪变量（key 以 `=` 开头），它们不是切片器给的，也看不懂，直接跳过。
/// 一律走 `vars_os` + `to_string_lossy`：Windows 环境变量不保证是合法 UTF-8，
/// 用 `vars()` 会在这里 panic，那就本末倒置了。排序交给 `render_snapshot`。
fn collect_vars() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| {
            let key = k.to_string_lossy().into_owned();
            if key.is_empty() || key.starts_with('=') {
                return None;
            }
            Some((key, v.to_string_lossy().into_owned()))
        })
        .collect()
}

/// 切片器注入的变量。`SLIC3R_` 是 Prusa/Orca 系的历史前缀，另外几个是各家自己的。
fn is_slicer_var(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    k.starts_with("SLIC3R_")
        || k.starts_with("ORCA")
        || k.starts_with("BAMBU")
        || k.starts_with("PRUSA")
        || k.starts_with("SUPERSLICER")
}

/// 可能带「输出文件名 / 导出路径」的变量。
///
/// 故意**不**把 `PATH`、`USERNAME`、`APPDATA` 这类系统变量算进来：它们每次都有，
/// 混进候选清单只会把真正有用的那几行淹掉。所以 `NAME` 只对切片器前缀生效。
/// 也不能用 `PP_` 这种短词：`WORKBUDDY_APP_*` 里的 `APP_` 会把它命中。
/// 切片器变量本来就整节单列了，这里不需要再靠 `PP_` 捞一遍。
fn is_path_hint(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    let output_words = [
        "OUTPUT",
        "GCODE",
        "EXPORT",
        "FILENAME",
        "FILEPATH",
        "FILE_NAME",
        "OUT_DIR",
    ];
    output_words.iter().any(|w| k.contains(w))
        || (is_slicer_var(key) && (k.contains("NAME") || k.contains("PATH")))
}

fn should_redact(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    [
        "PASSWORD",
        "TOKEN",
        "SECRET",
        "CREDENTIAL",
        "AUTH",
        "API_KEY",
        "ACCESS_KEY",
        "PRIVATE_KEY",
        "COOKIE",
        "_KEY",
    ]
    .iter()
    .any(|w| k.contains(w))
}

/// 值压成一行：切片器的配置偶尔是整段多行 JSON，不转义会把日志的行结构撕碎。
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn clip(value: &str, max: usize) -> String {
    let n = value.chars().count();
    if n <= max {
        return value.to_string();
    }
    let head: String = value.chars().take(max).collect();
    format!("{head}…（已截断，共 {n} 字符）")
}

fn display_value(key: &str, value: &str, full: bool) -> String {
    if !full && should_redact(key) {
        return format!(
            "<已脱敏，{} 字符；设 STARFISH_LOG_FULL_ENV=1 可看全值>",
            value.chars().count()
        );
    }
    clip(&escape(value), MAX_VALUE_CHARS)
}

// ── 渲染 ────────────────────────────────────────────────────────────────

struct SnapshotCtx {
    time: String,
    pid: u32,
    exe: String,
    cwd: String,
    argv: Vec<String>,
    target: String,
    full_env: bool,
    log_dir: String,
}

impl SnapshotCtx {
    fn current(target: &str, full_env: bool) -> Self {
        Self {
            time: pretty_now(),
            pid: std::process::id(),
            exe: std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<未知>".into()),
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<未知>".into()),
            // Orca 的工作目录和 exe 所在目录多半不是一回事，这个信息很关键
            argv: std::env::args_os()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
            target: target.to_string(),
            full_env,
            log_dir: log_dir().display().to_string(),
        }
    }
}

/// 把 `naming::timestamp_now()` 的 `20261111-130752` 摆成好读的形式。
fn pretty_now() -> String {
    let t = naming::timestamp_now();
    if t.len() == 15 {
        format!(
            "{}-{}-{} {}:{}:{}",
            &t[..4],
            &t[4..6],
            &t[6..8],
            &t[9..11],
            &t[11..13],
            &t[13..15]
        )
    } else {
        t
    }
}

const RULE: &str =
    "================================================================================\n";
const THIN: &str =
    "--------------------------------------------------------------------------------\n";

/// 纯函数：给一份变量表 + 上下文，产出日志文本。测试直接喂假数据，不碰进程环境。
///
/// 排序放在这里而不是 `collect_vars`：日志的呈现顺序必须由渲染层保证，
/// 将来任何别的调用方传进来乱序数据也不会写出一份难对照的日志。
fn render_snapshot(vars: &[(String, String)], ctx: &SnapshotCtx) -> String {
    let mut ordered = vars.to_vec();
    ordered.sort_by(|a, b| {
        a.0.to_ascii_uppercase()
            .cmp(&b.0.to_ascii_uppercase())
            .then(a.0.cmp(&b.0))
    });
    let vars = ordered.as_slice();
    let mut s = String::with_capacity(8 * 1024);
    s.push('\n');
    s.push_str(RULE);
    s.push_str(&format!(
        "[{}] 后处理进程环境快照   pid={}\n",
        ctx.time, ctx.pid
    ));
    s.push_str(THIN);
    s.push_str(&format!("  目标文件 : {}\n", ctx.target));
    s.push_str(&format!("  argv     : {:?}\n", ctx.argv));
    s.push_str(&format!("  cwd      : {}\n", ctx.cwd));
    s.push_str(&format!("  exe      : {}\n", ctx.exe));
    s.push_str(&format!("  日志目录 : {}\n", ctx.log_dir));
    s.push_str(&format!(
        "  脱敏     : {}\n",
        if ctx.full_env {
            "已关闭（全值）"
        } else {
            "开启"
        }
    ));

    let slicer: Vec<_> = vars.iter().filter(|(k, _)| is_slicer_var(k)).collect();
    let hints: Vec<_> = vars
        .iter()
        .filter(|(k, _)| is_path_hint(k) && !is_slicer_var(k))
        .collect();

    section(
        &mut s,
        &format!(
            "切片器注入的变量（SLIC3R_ / ORCA* / BAMBU* / PRUSA*）  共 {} 个",
            slicer.len()
        ),
        &slicer,
        ctx.full_env,
    );
    section(
        &mut s,
        &format!("其它可能含输出文件名/路径的变量  共 {} 个", hints.len()),
        &hints,
        ctx.full_env,
    );
    let all: Vec<_> = vars.iter().collect();
    section(
        &mut s,
        &format!("全部环境变量（按名称排序）  共 {} 个", all.len()),
        &all,
        ctx.full_env,
    );
    s
}

fn section(out: &mut String, title: &str, picked: &[&(String, String)], full: bool) {
    out.push_str(THIN);
    out.push_str(&format!("■ {title}\n"));
    if picked.is_empty() {
        out.push_str("  （无）\n");
        return;
    }
    for (k, v) in picked {
        out.push_str(&format!("  {k} = {}\n", display_value(k, v, full)));
    }
}

// ── 落盘 ────────────────────────────────────────────────────────────────

/// 追加一段文本到 `<dir>/starfish-control.log`，超上限先轮转。
fn append_to(dir: &Path, body: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(LOG_FILE);
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > MAX_LOG_BYTES {
            let prev = dir.join(PREV_FILE);
            let _ = fs::remove_file(&prev);
            let _ = fs::rename(&path, &prev); // 轮转失败不影响继续追加
        }
    }
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    f.write_all(body.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> SnapshotCtx {
        SnapshotCtx {
            time: "2026-11-11 13:07:52".into(),
            pid: 4242,
            exe: r"C:\bin\starfish-control.exe".into(),
            cwd: r"C:\Temp\orcaslicer_model\session".into(),
            argv: vec![
                r"C:\bin\starfish-control.exe".into(),
                r"C:\Temp\session\Metadata\.1.1.gcode.pp".into(),
            ],
            target: r"C:\Temp\session\Metadata\.1.1.gcode.pp".into(),
            full_env: false,
            log_dir: r"C:\Users\me\AppData\Local\starfish-control".into(),
        }
    }

    fn vars() -> Vec<(String, String)> {
        vec![
            ("SLIC3R_FILAMENT_TYPE".into(), "PETG".into()),
            (
                "SLIC3R_PP_OUTPUT_NAME".into(),
                "Part 1_PETG_6m4s.gcode".into(),
            ),
            ("ORCA_EXPORT_DIR".into(), r"C:\Users\me\Downloads".into()),
            ("PATH".into(), r"C:\Windows".into()),
            ("USERNAME".into(), "me".into()),
            ("MY_API_TOKEN".into(), "supersecret".into()),
        ]
    }

    #[test]
    fn slicer_prefixes_are_recognised() {
        assert!(is_slicer_var("SLIC3R_LAYER_HEIGHT"));
        assert!(is_slicer_var("orca_pp_output"));
        assert!(is_slicer_var("BAMBU_X"));
        assert!(is_slicer_var("PRUSASLICER_X"));
        assert!(!is_slicer_var("PATH"));
        assert!(!is_slicer_var("SLICER")); // 没有下划线后缀，不算
    }

    #[test]
    fn path_hints_skip_everyday_system_vars() {
        assert!(is_path_hint("SLIC3R_PP_OUTPUT_NAME"));
        assert!(is_path_hint("ORCA_EXPORT_DIR"));
        assert!(is_path_hint("MY_GCODE_FILE"));
        // 这几个每次都有，混进候选清单只会淹掉有用的行
        assert!(!is_path_hint("PATH"));
        assert!(!is_path_hint("USERNAME"));
        assert!(!is_path_hint("APPDATA"));
        // 短词 `PP_` 会把 `...APP_...` 全捞进来，实测踩过一次
        assert!(!is_path_hint("WORKBUDDY_APP_PATH"));
        assert!(!is_path_hint("WORKBUDDY_APP_NAME"));
    }

    #[test]
    fn sensitive_values_are_redacted_but_can_be_opted_out() {
        assert!(display_value("MY_API_TOKEN", "supersecret", false).contains("已脱敏"));
        assert_eq!(
            display_value("MY_API_TOKEN", "supersecret", true),
            "supersecret"
        );
        assert_eq!(display_value("SLIC3R_FILAMENT_TYPE", "PETG", false), "PETG");
    }

    #[test]
    fn multiline_values_stay_on_one_log_line() {
        let v = "line1\r\nline2\t\"quoted\"";
        assert_eq!(escape(v), "line1\\r\\nline2\\t\"quoted\"");
        let clipped = clip(&"x".repeat(20), 8);
        assert!(clipped.starts_with("xxxxxxxx…"), "{clipped}");
        assert!(clipped.contains("共 20 字符"));
    }

    #[test]
    fn snapshot_has_all_three_sections_and_sorted_full_list() {
        let text = render_snapshot(&vars(), &ctx());
        assert!(text.contains("切片器注入的变量"));
        assert!(text.contains("其它可能含输出文件名/路径的变量"));
        assert!(text.contains("全部环境变量"));
        assert!(text.contains("cwd      :"));
        assert!(text.contains("SLIC3R_PP_OUTPUT_NAME = Part 1_PETG_6m4s.gcode"));
        assert!(text.contains("MY_API_TOKEN = <已脱敏"));
        assert!(!text.contains("supersecret"), "脱敏后不该出现明文");

        // 全量小节里变量按名字排序：MY_API_TOKEN 在 ORCA_ 之后、PATH 之前
        let tail = text.split("■ 全部环境变量").nth(1).unwrap();
        let a = tail.find("MY_API_TOKEN").unwrap();
        let b = tail.find("PATH").unwrap();
        assert!(a < b, "全量清单没有排序");
    }

    #[test]
    fn empty_sections_say_so_instead_of_looking_broken() {
        let text = render_snapshot(&[], &ctx());
        assert_eq!(text.matches("（无）").count(), 3);
    }

    #[test]
    fn append_creates_dir_and_appends_in_order() {
        let dir = std::env::temp_dir().join(format!("starfish-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let p = append_to(&dir, "first\n").unwrap();
        append_to(&dir, "second\n").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "first\nsecond\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pretty_now_is_readable() {
        let t = pretty_now();
        assert_eq!(t.len(), 19, "{t}");
        assert_eq!(&t[4..5], "-");
        assert_eq!(&t[13..14], ":");
    }
}
