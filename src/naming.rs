//! 复现切片器给导出文件起的名字。
//!
//! # 为什么需要
//!
//! Orca 调后处理脚本时传的是**它内部的临时文件**，不是你要导出的那个文件：
//!
//! ```text
//! %TEMP%\orcaslicer_model\Tue_Nov_10\14_44_36#11596#237\Metadata\.11596.1.gcode.pp
//! ```
//!
//! 真实导出文件（`Downloads\Part 1 (16)_PETG_6m4s.gcode`）是**脚本退出之后**
//! 才由 Orca 写出来的，脚本里根本拿不到那个路径 —— 所以「先写真实文件再交给界面」
//! 这条路走不通。直接拿临时文件名当上传名，用户看到的就是 `.11596.1.gcode.pp`。
//!
//! # 怎么算
//!
//! Orca 在同一个会话目录里留了两样东西，恰好够我们把名字一模一样地算出来：
//!
//! | 文件 | 作用 |
//! |---|---|
//! | `<会话>/origin.txt` | 原始 3MF 的完整路径 → `input_filename_base` |
//! | `<会话>/_temp_<n>.config` | 打印配置里的 `filename_format` 和 `filament_type` |
//!
//! 模板长这样（用户在 Orca 里可以改，所以我们读它而不是硬编码）：
//!
//! ```text
//! {input_filename_base}_{filament_type[initial_tool]}_{print_time}.gcode
//! ```
//!
//! `print_time` 用 gcode 头里的 `; estimated printing time (normal mode) = 6m 4s`
//! 换算成 Orca 自己的短格式（`6m4s` / `1h21m`，**不带前导零**）。
//!
//! 三样缺一律返回 `None`：宁可退回原文件名，也不要拼出一个看起来像真的、
//! 实际对不上的名字 —— 那比 `xxx.pp` 更误导人。

use std::path::{Path, PathBuf};

use crate::protocol::header::HeaderInfo;

/// 按切片器的命名模板算出导出文件名（含扩展名）。任何一环拿不到就 `None`。
pub fn orca_export_name(gcode: &Path, info: &HeaderInfo) -> Option<String> {
    let session = find_session_dir(gcode)?;
    let base = origin_stem(&session)?;
    let config = read_config(&session)?;
    let format = json_string(&config, "filename_format")?;
    let filament = json_first_array_string(&config, "filament_type").unwrap_or_default();
    let time = if info.time_s > 0.0 {
        orca_short_time(info.time_s)
    } else {
        String::new()
    };

    let name = apply_format(&format, &base, &filament, &time)?;
    // 只接受真的以 .gcode 结尾的结果：模板被用户改坏时不要冒险。
    if !name.to_ascii_lowercase().ends_with(".gcode") || name.len() > 200 {
        return None;
    }
    Some(name)
}

/// 只要拿得到原始 3MF 名，至少能给个像样的名字（`Part 1 (16).gcode`）。
///
/// 模板解析失败时用这个兜底，比 `.11596.1.gcode.pp` 强得多。
pub fn origin_based_name(gcode: &Path) -> Option<String> {
    let session = find_session_dir(gcode)?;
    let base = origin_stem(&session)?;
    Some(format!("{base}.gcode"))
}

/// 从 gcode 往上找带 `origin.txt` 的会话目录（`Metadata` → 会话目录，一层就够，
/// 多留几层是防 Orca 以后改成更深的嵌套）。
fn find_session_dir(gcode: &Path) -> Option<PathBuf> {
    let mut dir = gcode.parent()?;
    for _ in 0..4 {
        if dir.join("origin.txt").is_file() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
    None
}

fn origin_stem(session: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(session.join("origin.txt")).ok()?;
    let path = Path::new(raw.trim());
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    if stem.is_empty() {
        None
    } else {
        Some(stem)
    }
}

fn read_config(session: &Path) -> Option<String> {
    for entry in std::fs::read_dir(session).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("_temp_") && name.ends_with(".config") {
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                return Some(text);
            }
        }
    }
    None
}

/// 取 `"key": "value"` 里的 value。
///
/// 只做最小解析：这些 config 是 Orca 自己生成的规范 JSON，不需要完整解析器，
/// 也就不用为几行代码背一个 serde_json 依赖。
fn json_string(text: &str, key: &str) -> Option<String> {
    let head = text.find(&format!("\"{key}\""))?;
    let rest = &text[head..];
    let rest = &rest[rest.find(':')? + 1..];
    let rest = &rest[rest.find('"')? + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// 取 `"key": [ "first", ... ]` 里的第一个元素。
fn json_first_array_string(text: &str, key: &str) -> Option<String> {
    let head = text.find(&format!("\"{key}\""))?;
    let rest = &text[head..];
    let rest = &rest[rest.find('[')? + 1..];
    let rest = &rest[rest.find('"')? + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// 按模板拼名字。遇到不认识的变量就放弃（`None`），不猜。
fn apply_format(format: &str, base: &str, filament: &str, time: &str) -> Option<String> {
    let mut out = String::with_capacity(format.len() + base.len() + 16);
    let mut rest = format;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        let close = tail.find('}')?;
        let key = &tail[1..close];
        let value = if key == "input_filename_base" {
            base
        } else if key.starts_with("filament_type") {
            if filament.is_empty() {
                return None; // 模板要材料名却没有，拼出来会是 `_ _`
            }
            filament
        } else if key == "print_time" {
            if time.is_empty() {
                return None;
            }
            time
        } else {
            return None; // 不认识的变量
        };
        out.push_str(value);
        rest = &tail[close + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// Orca 的短时长格式：`6m4s` / `1h21m`（**不带前导零**，和 `header::fmt_duration` 不同）。
///
/// 名字里差一个 `0` 就对不上真实文件名，所以这里单独实现一份。
pub fn orca_short_time(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

// ── 上传名加时间戳 ──────────────────────────────────────────────────────
//
// 设备端有个坑：**同名文件不会覆盖，反而会照旧打旧的**（用户实测）。
// 同一个模型切第二次、名字完全一样时，新内容传上去，开始打印却还是上一次的
// 刀路 —— 这种错很难看出来，因为文件列表、大小、进度全都"正常"。
//
// 所以每次上传都缀一个时间戳，让名字天然唯一。代价是机内文件会累积，
// 但这比"打错文件"划算得多，而且列表能靠时间戳一眼分清是哪次导出的。

/// 设备端文件名上限（和 `app::sanitize_name` 的 `MAX` 保持一致）。
const NAME_MAX: usize = 64;

/// 在扩展名前插入 `_<stamp>`：`a_PETG_6m4s.gcode` → `a_PETG_6m4s_20261110-154030.gcode`。
///
/// 超长时**优先砍主体、保时间戳和扩展名**：时间戳是唯一性的保证，
/// 砍掉它就等于没加。名字已经过 `sanitize_name`，是纯 ASCII，按字节算长度即可。
pub fn stamp(filename: &str, stamp: &str) -> String {
    let (stem, ext) = match filename.rfind('.') {
        Some(i) if i > 0 && i + 1 < filename.len() => (&filename[..i], &filename[i..]),
        _ => (filename, ""),
    };
    let tail = format!("_{stamp}{ext}");
    let budget = NAME_MAX.saturating_sub(tail.len());
    let stem = if stem.len() <= budget {
        stem.to_string()
    } else {
        // 掐头保尾：模型名尾部通常还有材料/时长，比开头更有信息量。
        // 预算实在太小就整段丢掉，宁可名字难认，也不能超过设备上限。
        stem[stem.len() - budget..].to_string()
    };
    // 点开头 = 设备上的「隐藏文件」：列表接口不会列出来，传上去了也看不见、选不到。
    // 切片器的中间文件恰好是 `.11596.1.gcode` 这种形状，所以这里再去一次点前缀
    // （`sanitize_name` 也会去，这里是防自己以后绕过它单独调用）。
    let stem = stem.trim_start_matches('.').trim_matches('_');
    let stem = if stem.is_empty() { "upload" } else { stem };
    format!("{stem}{tail}")
}

/// 当前本地时间的时间戳 `YYYYMMDD-HHmmss`。
pub fn timestamp_now() -> String {
    let (y, mo, d, h, mi, s) = local_now();
    fmt_stamp(y, mo, d, h, mi, s)
}

fn fmt_stamp(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> String {
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

#[cfg(windows)]
fn local_now() -> (i32, u32, u32, u32, u32, u32) {
    // 不想为了一次取时间背 chrono；std 也没有本地时区偏移，直接问系统。
    #[repr(C)]
    #[derive(Default)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        ms: u16,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetLocalTime(t: *mut SystemTime);
    }
    let mut t = SystemTime::default();
    unsafe { GetLocalTime(&mut t) };
    if t.year == 0 {
        return utc_now();
    }
    (
        t.year as i32,
        t.month as u32,
        t.day as u32,
        t.hour as u32,
        t.minute as u32,
        t.second as u32,
    )
}

#[cfg(not(windows))]
fn local_now() -> (i32, u32, u32, u32, u32, u32) {
    utc_now()
}

fn utc_now() -> (i32, u32, u32, u32, u32, u32) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    (
        y,
        mo,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// 1970-01-01 起的天数 → 公历年月日（Howard Hinnant 的 civil_from_days）。
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_session(tag: &str, origin: &str, format: &str, filament: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("starfish-naming-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Metadata")).unwrap();
        std::fs::write(dir.join("origin.txt"), origin).unwrap();
        let config = format!(
            "{{\n\t\"filename_format\": \"{format}\",\n\t\"filament_type\": [\n\t\t\"{filament}\",\n\t\t\"{filament}\"\n\t]\n}}"
        );
        std::fs::write(dir.join("_temp_1.config"), config).unwrap();
        dir
    }

    fn info_with(seconds: f64) -> HeaderInfo {
        HeaderInfo {
            time_s: seconds,
            ..Default::default()
        }
    }

    #[test]
    fn reproduces_the_real_orca_name() {
        // 真机上的实测样例：Part 1 (16).3mf + PETG + 6m4s
        let dir = fake_session(
            "real",
            r"C:\Users\andel\Downloads\Part 1 (16).3mf",
            "{input_filename_base}_{filament_type[initial_tool]}_{print_time}.gcode",
            "PETG",
        );
        let gcode = dir.join("Metadata").join(".11596.1.gcode.pp");
        let name = orca_export_name(&gcode, &info_with(364.0)).expect("应该算得出来");
        assert_eq!(name, "Part 1 (16)_PETG_6m4s.gcode");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn falls_back_when_template_has_unknown_variables() {
        let dir = fake_session(
            "unknown",
            r"D:\x\Pin (2).3mf",
            "{input_filename_base}_{filament_type[0]}_{print_time}_v2.gcode",
            "PLA",
        );
        let gcode = dir.join("Metadata").join("a.gcode.pp");
        // `_v2` 是普通文本，能过；换一个真不认识的变量应该放弃
        assert!(orca_export_name(&gcode, &info_with(60.0)).is_some());
        let gcode2 = dir.join("Metadata").join("b.gcode.pp");
        std::fs::write(
            dir.join("_temp_1.config"),
            r#"{"filename_format": "{input_filename_base}_{layer_height}.gcode", "filament_type": ["PLA"]}"#,
        )
        .unwrap();
        assert_eq!(orca_export_name(&gcode2, &info_with(60.0)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn origin_based_name_works_without_config() {
        let dir = std::env::temp_dir().join(format!("starfish-naming-{}-nocfg", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Metadata")).unwrap();
        std::fs::write(dir.join("origin.txt"), r"D:\导出\Pin (2).3mf").unwrap();
        let gcode = dir.join("Metadata").join(".1.1.gcode.pp");
        assert_eq!(
            origin_based_name(&gcode).as_deref(),
            Some("Pin (2).gcode")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_time_matches_orca() {
        assert_eq!(orca_short_time(364.0), "6m4s");
        assert_eq!(orca_short_time(4860.0), "1h21m");
        assert_eq!(orca_short_time(45.0), "45s");
        assert_eq!(orca_short_time(0.0), "0s");
    }

    #[test]
    fn stamp_goes_before_the_extension() {
        assert_eq!(
            stamp("Part 1 (16)_PETG_6m4s.gcode", "20261110-154030"),
            "Part 1 (16)_PETG_6m4s_20261110-154030.gcode"
        );
        // 没有扩展名也不能把时间戳贴到奇怪的地方
        assert_eq!(stamp("noext", "20261110-154030"), "noext_20261110-154030");
        // 点开头是设备上的隐藏文件，必须去掉（否则列表里根本不列出来）
        assert_eq!(
            stamp(".11596.1.gcode", "20261110-154030"),
            "11596.1_20261110-154030.gcode"
        );
    }

    #[test]
    fn stamp_keeps_name_within_device_limit() {
        let long = format!("{}.gcode", "A".repeat(80));
        let out = stamp(&long, "20261110-154030");
        assert!(out.len() <= NAME_MAX, "长度 {} 超过上限", out.len());
        assert!(out.ends_with("_20261110-154030.gcode"), "时间戳或扩展名被砍了：{out}");
        assert!(!out.starts_with('_'));
    }

    #[test]
    fn stamp_format_is_fixed_width() {
        assert_eq!(fmt_stamp(2026, 11, 10, 15, 40, 3), "20261110-154003");
        assert_eq!(fmt_stamp(2026, 1, 2, 3, 4, 5), "20260102-030405");
    }

    #[test]
    fn civil_from_days_matches_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
    }

    #[test]
    fn timestamp_now_looks_like_a_timestamp() {
        let t = timestamp_now();
        assert_eq!(t.len(), 15, "格式应为 YYYYMMDD-HHmmss，实际 {t}");
        assert_eq!(&t[8..9], "-");
        assert!(t[..8].chars().all(|c| c.is_ascii_digit()), "{t}");
        assert!(t[9..].chars().all(|c| c.is_ascii_digit()), "{t}");
        // 年份落在合理区间：写死这个断言是为了防止时区/换算整个算错还"看起来正常"
        let year: i32 = t[..4].parse().unwrap();
        assert!((2020..=2100).contains(&year), "年份不对劲：{t}");
    }
}
