//! Starfish A6 G-code 头部后处理（`starfish_a6_header.py` 的 Rust 移植）。
//!
//! 把 OrcaSlicer / PrusaSlicer 的 G-code 头部改写成打印机固件认识的 `;Header Start`
//! 块：缩略图、预计时间、温度、耗材长度/重量、包围盒等。
//!
//! 与 Python 版的三处重要差别（都是原脚本的真实 bug）：
//! 1. **缩略图取最大那块**。Orca 会写好几段 `; thumbnail begin WxH`（48x48 在前、
//!    300x300 在后），原脚本用 `re.search` 拿第一个 → 永远是小的那张。
//! 2. **解析时长**。Orca 写的是 `; estimated printing time (normal mode) = 2h 14m 6s`，
//!    原脚本的正则只认 `123s`，于是 `;TIME:` 变成 0。
//! 3. **键值对不吃空格差异**。Orca 写 `; filament used [mm] = 16143.76`（分号后有空格），
//!    原脚本正则写成 `;filament used` 匹配不上 → 耗材变 0。这里统一归一化键名，
//!    `Layer height` / `layer_height`、`;X:` / `; X = ` 都能命中。
//!
//! 纯 std 实现（无 regex 依赖）：单遍扫行 + 哈希表式键值收集。

use std::collections::HashMap;

/// 机型名，写进 `;machine:`。
pub const MACHINE_NAME: &str = "Starfish A6";
/// 头部类型，写进 `;header_type:`。
pub const HEADER_TYPE: &str = "3dp";
/// 工具头标识。
pub const TOOL_HEAD: &str = "singleExtruderToolheadForOriginal";
/// 0 号喷嘴直径（mm）。
pub const NOZZLE_0_DIAMETER: f64 = 0.4;
/// 1 号喷嘴直径：-1 表示没有。
pub const NOZZLE_1_DIAMETER: f64 = -1.0;
/// 1 号喷嘴温度：-1 表示没有。
pub const NOZZLE_1_TEMPERATURE: f64 = -1.0;
/// 1 号喷嘴材料：空。
pub const NOZZLE_1_MATERIAL: &str = "";
/// 默认工作速度（mm/min）。
pub const WORK_SPEED_MM_MIN: f64 = 3000.0;

/// 头部行数（`;Header Start` … `;Header End` + 末尾空行）。
const HEADER_LINES: usize = 38;

/// XYZ 包围盒（mm）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Bounds {
    pub min_x: f64,
    pub min_y: f64,
    pub min_z: f64,
    pub max_x: f64,
    pub max_y: f64,
    pub max_z: f64,
}

/// 从 G-code 里取出的缩略图。
#[derive(Debug, Clone)]
pub struct Thumbnail {
    /// 图像宽度（像素）。
    pub width: u32,
    /// 图像高度（像素）。
    pub height: u32,
    /// 解码后的 PNG 字节，可直接交给 UI 显示。
    pub png: Vec<u8>,
    /// 来源描述，例如 `block 300x300` / `inline`。
    pub source: String,
}

impl Thumbnail {
    /// 像素面积，用来在多个缩略图里挑最大的。
    pub fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

/// 生成的头部里那些有用的信息（UI 用来展示）。
#[derive(Debug, Clone, Default)]
pub struct HeaderInfo {
    pub time_s: f64,
    pub layer_height: f64,
    pub nozzle_temp: f64,
    pub bed_temp: f64,
    pub material: String,
    /// 耗材长度（m）。
    pub length_m: f64,
    /// 耗材重量（g）。
    pub weight_g: f64,
    pub bounds: Bounds,
    /// 写入 `;file_total_lines:` 的值。
    pub line_count: usize,
    pub thumbnail: Option<Thumbnail>,
}

impl HeaderInfo {
    /// 一行中文摘要，给状态栏 / 弹窗用。
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.time_s > 0.0 {
            parts.push(fmt_duration(self.time_s));
        }
        if self.nozzle_temp > 0.0 || self.bed_temp > 0.0 {
            parts.push(format!(
                "喷头 {:.0}℃ / 热床 {:.0}℃",
                self.nozzle_temp, self.bed_temp
            ));
        }
        if !self.material.is_empty() {
            parts.push(self.material.clone());
        }
        if self.weight_g > 0.0 {
            parts.push(format!("{:.1} g", self.weight_g));
        } else if self.length_m > 0.0 {
            parts.push(format!("{:.2} m", self.length_m));
        }
        let b = self.bounds;
        let size = (b.max_x - b.min_x, b.max_y - b.min_y, b.max_z - b.min_z);
        if size.0 > 0.0 || size.1 > 0.0 || size.2 > 0.0 {
            parts.push(format!("{:.0}×{:.0}×{:.0} mm", size.0, size.1, size.2));
        }
        if parts.is_empty() {
            "未取到元信息".to_string()
        } else {
            parts.join(" · ")
        }
    }
}

/// 转换结果。
#[derive(Debug, Clone)]
pub struct Conversion {
    /// 改写后的完整 G-code 文本（可原样上传）。
    pub output: String,
    pub info: HeaderInfo,
}

/// 把秒数写成 `2h48m` / `26m12s` / `45s` 这种紧凑形式（剩余时间、总时长共用）。
pub fn fmt_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// 键名归一化：小写、下划线换空格、压缩空白。
fn norm_key(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_space = false;
    for ch in raw.trim().chars() {
        let c = if ch == '_' { ' ' } else { ch };
        let c = c.to_ascii_lowercase();
        if c.is_whitespace() {
            if !last_space && !out.is_empty() {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// 从一行注释里切成 `(归一化键, 值)`。
///
/// 分隔符取 `:` 与 `=` 里更早出现的那个；两样都没有就不是键值行。
fn parse_kv(line: &str) -> Option<(String, String)> {
    let body = line.trim_start();
    let body = body.strip_prefix(';')?.trim_start();
    if body.is_empty() {
        return None;
    }
    let colon = body.find(':');
    let equals = body.find('=');
    let idx = match (colon, equals) {
        (Some(c), Some(e)) => c.min(e),
        (Some(c), None) => c,
        (None, Some(e)) => e,
        (None, None) => return None,
    };
    let key = norm_key(&body[..idx]);
    if key.is_empty() || key.len() > 48 {
        return None;
    }
    let value = body[idx + 1..].trim();
    Some((key, value.to_string()))
}

/// 取字符串开头的数字，容忍后面的单位（`3.62m`、`255`、`0.25mm`）。
fn leading_number(s: &str) -> Option<f64> {
    let s = s.trim();
    let bytes = s.as_bytes();
    let mut end = 0;
    let mut seen_dot = false;
    if end < bytes.len() && (bytes[end] == b'-' || bytes[end] == b'+') {
        end += 1;
    }
    while end < bytes.len() {
        let c = bytes[end];
        if c.is_ascii_digit() {
            end += 1;
        } else if c == b'.' && !seen_dot {
            seen_dot = true;
            end += 1;
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    s[..end].parse::<f64>().ok()
}

/// 解析 `1h 2m 3s` / `26s` / `7300` / `2h14m6s`，返回秒。
fn parse_duration(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // 纯数字：当作秒。
    if let Ok(v) = s.parse::<f64>() {
        return Some(v);
    }
    let lower = s.to_ascii_lowercase();
    let mut total = 0.0f64;
    let mut found = false;
    let mut num = String::new();
    for ch in lower.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num.push(ch);
            continue;
        }
        match ch {
            'h' => {
                if let Ok(v) = num.parse::<f64>() {
                    total += v * 3600.0;
                    found = true;
                }
                num.clear();
            }
            'm' => {
                if let Ok(v) = num.parse::<f64>() {
                    total += v * 60.0;
                    found = true;
                }
                num.clear();
            }
            's' => {
                if let Ok(v) = num.parse::<f64>() {
                    total += v;
                    found = true;
                }
                num.clear();
            }
            _ => num.clear(),
        }
    }
    if found {
        Some(total)
    } else {
        None
    }
}

/// 标准 Base64 解码（忽略空白；`=` 之后不再接受数据）。
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(s.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let mut padded = false;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                padded = true;
                continue;
            }
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        if padded {
            return None;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
            if bits == 0 {
                acc = 0;
            } else {
                acc &= (1u32 << bits) - 1;
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 读 PNG 的 IHDR 尺寸（第 16..24 字节，大端）。
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    if w == 0 || h == 0 {
        None
    } else {
        Some((w, h))
    }
}

/// 缩略图边长上限：打印机屏幕用不满更大的，头部也能压在一行里。
const THUMB_MAX: u32 = 300;

/// 抽出所有缩略图**原始字节**（`; thumbnail begin WxH` 块 + 旧 Starfish 头里的 inline data URI）。
///
/// 这里只负责「切出候选」，不判断好坏——格式可能是 PNG / BMP / JPG，
/// 声明尺寸也可能和真实尺寸不一致，一律交给 `image` 真解码后再比。
fn scan_thumbnail_blobs(text: &str) -> Vec<(Vec<u8>, String)> {
    let mut out: Vec<(Vec<u8>, String)> = Vec::new();
    let mut in_block = false;
    let mut declared = (0u32, 0u32);
    let mut payload = String::new();

    for line in text.lines() {
        let t = line.trim_start();
        let Some(comment) = t.strip_prefix(';') else {
            continue;
        };
        let comment = comment.trim_start();

        if !in_block {
            // inline：`;thumbnail: data:image/xxx;base64,xxxxx`
            if let Some((key, value)) = parse_kv(line) {
                if key == "thumbnail" {
                    if let Some(pos) = value.find("base64,") {
                        if let Some(bytes) = b64_decode(&value[pos + 7..]) {
                            out.push((bytes, "inline".to_string()));
                        }
                    }
                    continue;
                }
            }
            // 块开始：`thumbnail begin WxH len`
            let lower = comment.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("thumbnail") {
                let rest = rest.trim_start();
                if let Some(args) = rest.strip_prefix("begin") {
                    let size = args.split_whitespace().next().unwrap_or("");
                    let mut it = size.split('x');
                    declared = (
                        it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
                        it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
                    );
                    in_block = true;
                    payload.clear();
                }
            }
            continue;
        }

        // 块内
        if comment.to_ascii_lowercase().starts_with("thumbnail end") {
            if let Some(bytes) = b64_decode(&payload) {
                out.push((bytes, format!("block {}x{}", declared.0, declared.1)));
            }
            in_block = false;
            payload.clear();
            continue;
        }
        for ch in comment.chars() {
            if !ch.is_whitespace() {
                payload.push(ch);
            }
        }
    }
    out
}

/// 把一张图归一化：超过上限就缩放，统一写成 PNG。
fn normalize_thumb(img: image::DynamicImage, source: String) -> Option<Thumbnail> {
    let (w, h) = (img.width(), img.height());
    let img = if w > THUMB_MAX || h > THUMB_MAX {
        img.resize(THUMB_MAX, THUMB_MAX, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    let mut cursor = std::io::Cursor::new(Vec::new());
    img.write_to(&mut cursor, image::ImageFormat::Png).ok()?;
    let png = cursor.into_inner();
    let (fw, fh) = png_size(&png).unwrap_or((img.width(), img.height()));
    Some(Thumbnail {
        width: fw,
        height: fh,
        png,
        source,
    })
}

/// 在所有候选里挑**最合适**的那张。
///
/// 判据是「解码后的真实像素面积最大」——不是声明尺寸，也不是文件里出现的先后。
/// Orca 把 48x48 写在前面、300x300 写在后面，按先后取就会拿到糊的那张（原脚本的 bug）。
/// 解码失败的候选直接淘汰；最终统一转 PNG 并限制在 `THUMB_MAX` 以内。
fn pick_best_thumbnail(text: &str) -> Option<Thumbnail> {
    let mut best: Option<(u64, image::DynamicImage, String)> = None;
    for (bytes, source) in scan_thumbnail_blobs(text) {
        let img = match image::load_from_memory(&bytes) {
            Ok(i) => i,
            Err(_) => continue,
        };
        let area = img.width() as u64 * img.height() as u64;
        if best.as_ref().map_or(true, |(a, _, _)| area > *a) {
            best = Some((area, img, source));
        }
    }
    let (_, img, source) = best?;
    normalize_thumb(img, source)
}

/// 收集全文里的 `; key: value` / `; key = value`，先出现的优先（与 Python 的 search 一致）。
fn collect_kv(text: &str) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    for line in text.lines() {
        if !line.trim_start().starts_with(';') {
            continue;
        }
        if let Some((k, v)) = parse_kv(line) {
            map.entry(k).or_insert(v);
        }
    }
    map
}

/// 按键优先级取值并转数字。
fn pick_number(map: &HashMap<String, String>, keys: &[&str]) -> Option<f64> {
    for key in keys {
        let nk = norm_key(key);
        if let Some(v) = map.get(&nk) {
            if let Some(n) = leading_number(v) {
                return Some(n);
            }
        }
    }
    None
}

/// 按键优先级取原始字符串。
fn pick_value<'a>(map: &'a HashMap<String, String>, keys: &[&str]) -> Option<&'a str> {
    for key in keys {
        let nk = norm_key(key);
        if let Some(v) = map.get(&nk) {
            if !v.is_empty() {
                return Some(v.as_str());
            }
        }
    }
    None
}

/// 从 M104/M109/M140/M190 里扫目标温度（注释里没有设置时兜底）。
fn scan_body_temp(body: &[&str], cmds: &[&str]) -> Option<f64> {
    for line in body {
        let code = match line.find(';') {
            Some(i) => &line[..i],
            None => line,
        }
        .trim();
        let upper = code.to_ascii_uppercase();
        for cmd in cmds {
            if let Some(rest) = upper.strip_prefix(cmd) {
                if !rest.is_empty() && !rest.starts_with(' ') {
                    continue;
                }
                // 找 S 参数
                for tok in rest.split_whitespace() {
                    if let Some(v) = tok.strip_prefix('S') {
                        if let Some(n) = leading_number(v) {
                            return Some(n);
                        }
                    }
                }
            }
        }
    }
    None
}

/// 扫 G0/G1 求包围盒（注释里没有 MINX..MAXZ 时兜底）。
fn scan_bounds(body: &[&str]) -> Bounds {
    let (mut x, mut y, mut z) = (0.0f64, 0.0f64, 0.0f64);
    let mut absolute = true;
    let mut min = (f64::INFINITY, f64::INFINITY, f64::INFINITY);
    let mut max = (f64::NEG_INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);

    for line in body {
        let code = match line.find(';') {
            Some(i) => &line[..i],
            None => line,
        }
        .trim();
        if code.is_empty() {
            continue;
        }
        let upper = code.to_ascii_uppercase();

        if upper == "G90" || upper.starts_with("G90 ") {
            absolute = true;
            continue;
        }
        if upper == "G91" || upper.starts_with("G91 ") {
            absolute = false;
            continue;
        }
        if upper == "G92" || upper.starts_with("G92 ") {
            // G92 只是改坐标原点，不算移动。
            for (axis, slot) in [("X", 0usize), ("Y", 1), ("Z", 2)] {
                if let Some(idx) = upper.find(axis) {
                    if let Some(n) = leading_number(&upper[idx + 1..]) {
                        match slot {
                            0 => x = n,
                            1 => y = n,
                            _ => z = n,
                        }
                    }
                }
            }
            continue;
        }
        if !(upper.starts_with("G1 ") || upper.starts_with("G01 ") || upper == "G1") {
            continue;
        }
        for (axis, slot) in [("X", 0usize), ("Y", 1), ("Z", 2)] {
            let Some(idx) = upper.find(axis) else {
                continue;
            };
            let Some(n) = leading_number(&upper[idx + 1..]) else {
                continue;
            };
            let old = match slot {
                0 => x,
                1 => y,
                _ => z,
            };
            let new = if absolute || old == 0.0 { n } else { old + n };
            match slot {
                0 => x = new,
                1 => y = new,
                _ => z = new,
            }
        }
        min.0 = min.0.min(x);
        min.1 = min.1.min(y);
        min.2 = min.2.min(z);
        max.0 = max.0.max(x);
        max.1 = max.1.max(y);
        max.2 = max.2.max(z);
    }

    if !min.0.is_finite() {
        return Bounds::default();
    }
    Bounds {
        min_x: min.0,
        min_y: min.1,
        min_z: min.2,
        max_x: max.0,
        max_y: max.1,
        max_z: max.2,
    }
}

/// 数字格式化：去掉多余的 0 和点（与 Python 的 `fmt_num` 一致）。
fn fmt_num(v: f64) -> String {
    if !v.is_finite() {
        return "-1".to_string();
    }
    let mut s = format!("{v:.6}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s.is_empty() || s == "-0" {
        "0".to_string()
    } else {
        s
    }
}

/// 把 G-code 文本转成 Starfish 头部 + 原文件正文。
///
/// - `material_override`：命令行给了 `--material` 时优先用它。
pub fn convert(text: &str, material_override: Option<&str>) -> Conversion {
    let lines: Vec<&str> = text.lines().collect();

    // ① 找第一条真正的指令：之前全是注释/空行，都属于「头部」。
    let first_cmd = lines
        .iter()
        .position(|l| {
            let s = l.trim();
            !s.is_empty() && !s.starts_with(';')
        })
        .unwrap_or(lines.len());
    let body: &[&str] = &lines[first_cmd..];

    // ② 元信息
    let map = collect_kv(text);

    let mut time_s = pick_number(&map, &["time", "estimated_time(s)", "estimated time (s)"])
        .unwrap_or(0.0);
    if time_s <= 0.0 {
        if let Some(v) = pick_value(
            &map,
            &[
                "estimated printing time (normal mode)",
                "estimated printing time",
                "estimated printing time (silent mode)",
            ],
        ) {
            time_s = parse_duration(v).unwrap_or(0.0);
        }
    }

    let layer_height = pick_number(&map, &["layer height", "layer_height"]).unwrap_or(0.0);

    let mut nozzle_temp = pick_number(
        &map,
        &[
            "nozzle_temperature(°c)",
            "nozzle temperature(°c)",
            "nozzle_temperature",
            "nozzle temperature",
            "nozzle_temperature_initial_layer",
            "nozzle temperature initial layer",
        ],
    );
    if nozzle_temp.is_none() {
        nozzle_temp = scan_body_temp(body, &["M104", "M109"]);
    }

    let mut bed_temp = pick_number(
        &map,
        &[
            "build_plate_temperature(°c)",
            "build plate temperature(°c)",
            "hot_plate_temp_initial_layer",
            "hot plate temp initial layer",
            "hot_plate_temp",
            "bed_temperature",
            "bed temperature",
        ],
    );
    if bed_temp.is_none() {
        bed_temp = scan_body_temp(body, &["M140", "M190"]);
    }

    let material = match material_override {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => pick_value(
            &map,
            &["nozzle_0_material", "filament_type", "filament preset"],
        )
        .unwrap_or("")
        .to_string(),
    };

    // 长度：优先 --length，其次 [mm]、再次 `filament used: 3.62m`。
    let length_m = {
        if let Some(mm) = pick_number(&map, &["filament used [mm]"]) {
            mm / 1000.0
        } else if let Some(v) = pick_value(&map, &["filament used", "matierial_length(m)"]) {
            leading_number(v).unwrap_or(0.0)
        } else if let Some(mm) = pick_number(&map, &["filament used"]) {
            mm / 1000.0
        } else {
            0.0
        }
    };

    let weight_g = pick_number(
        &map,
        &[
            "matierial_weight",
            "filament used [g]",
            "total filament weight [g]",
            "total filament used [g]",
        ],
    )
    .unwrap_or(0.0);

    // ③ 包围盒：先信切片器写的 MINX..MAXZ（有哨兵值时弃用），否则扫 G0/G1。
    let sentinel = 2.14748e6;
    let commented = [
        pick_number(&map, &["minx"]),
        pick_number(&map, &["miny"]),
        pick_number(&map, &["minz"]),
        pick_number(&map, &["maxx"]),
        pick_number(&map, &["maxy"]),
        pick_number(&map, &["maxz"]),
    ];
    let bounds = match commented {
        [Some(minx), Some(miny), Some(minz), Some(maxx), Some(maxy), Some(maxz)]
            if minx.abs() < sentinel
                && miny.abs() < sentinel
                && maxx.abs() < sentinel
                && maxy.abs() < sentinel
                && minx <= maxx
                && miny <= maxy
                && minz <= maxz =>
        {
            Bounds {
                min_x: minx,
                min_y: miny,
                min_z: minz,
                max_x: maxx,
                max_y: maxy,
                max_z: maxz,
            }
        }
        _ => scan_bounds(body),
    };

    // ④ 缩略图：真解码后按像素面积取最大的那块，再统一转 PNG。
    let thumbnail = pick_best_thumbnail(text);

    // ⑤ 正文（去掉首尾空行，保证以单个 \n 结尾）
    let body_text = body.join("\n");
    let body_text = body_text.trim_start_matches(['\r', '\n']);
    let mut output = String::with_capacity(text.len() + 2048);
    // `;file_total_lines` 写在头部里，但头部的**行数**是固定的，
    // 所以「头部行数 + 正文行数」一次就能算准，不需要 Python 那种反复迭代。
    let line_count = HEADER_LINES + body.len();

    let thumb_line = match &thumbnail {
        Some(t) => format!(
            ";thumbnail: data:image/png;base64,{}",
            base64_encode::encode(&t.png)
        ),
        None => ";thumbnail:".to_string(),
    };

    let header: [String; HEADER_LINES] = [
        ";Header Start".into(),
        ";FLAVOR:Marlin".into(),
        format!(";TIME:{}", time_s.round() as i64),
        format!(";Filament used: {length_m:.5}m"),
        format!(";Layer height: {}", fmt_num(layer_height)),
        format!(";MINX:{}", fmt_num(bounds.min_x)),
        format!(";MINY:{}", fmt_num(bounds.min_y)),
        format!(";MINZ:{}", fmt_num(bounds.min_z)),
        format!(";MAXX:{}", fmt_num(bounds.max_x)),
        format!(";MAXY:{}", fmt_num(bounds.max_y)),
        format!(";MAXZ:{}", fmt_num(bounds.max_z)),
        String::new(),
        format!(";header_type: {HEADER_TYPE}"),
        format!(";tool_head: {TOOL_HEAD}"),
        format!(";machine: {MACHINE_NAME}"),
        thumb_line,
        format!(";file_total_lines: {line_count}"),
        format!(";estimated_time(s): {time_s:.2}"),
        format!(";nozzle_temperature(°C): {}", fmt_num(nozzle_temp.unwrap_or(0.0))),
        format!(";nozzle_1_temperature(°C): {}", fmt_num(NOZZLE_1_TEMPERATURE)),
        format!(";nozzle_0_diameter(mm): {}", fmt_num(NOZZLE_0_DIAMETER)),
        format!(";nozzle_1_diameter(mm): {}", fmt_num(NOZZLE_1_DIAMETER)),
        format!(";build_plate_temperature(°C): {}", fmt_num(bed_temp.unwrap_or(0.0))),
        format!(";work_speed(mm/minute): {}", fmt_num(WORK_SPEED_MM_MIN)),
        format!(";max_x(mm): {}", fmt_num(bounds.max_x)),
        format!(";max_y(mm): {}", fmt_num(bounds.max_y)),
        format!(";max_z(mm): {}", fmt_num(bounds.max_z)),
        format!(";min_x(mm): {}", fmt_num(bounds.min_x)),
        format!(";min_y(mm): {}", fmt_num(bounds.min_y)),
        format!(";min_z(mm): {}", fmt_num(bounds.min_z)),
        ";layer_number: 0".into(),
        format!(";layer_height: {}", fmt_num(layer_height)),
        format!(";matierial_weight: {}", fmt_num(weight_g)),
        format!(";matierial_length: {length_m:.5}"),
        format!(";nozzle_0_material: {material}"),
        format!(";nozzle_1_material: {NOZZLE_1_MATERIAL}"),
        ";Header End".into(),
        String::new(),
    ];

    for line in header.iter() {
        output.push_str(line);
        output.push('\n');
    }
    output.push_str(body_text);
    output.push('\n');

    Conversion {
        output,
        info: HeaderInfo {
            time_s,
            layer_height,
            nozzle_temp: nozzle_temp.unwrap_or(0.0),
            bed_temp: bed_temp.unwrap_or(0.0),
            material,
            length_m,
            weight_g,
            bounds,
            line_count,
            thumbnail,
        },
    }
}

/// 最小的 Base64 编码器（只为把缩略图写回 `;thumbnail:` 行，避免引外部 crate）。
mod base64_encode {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[(n >> 18) as usize & 63] as char);
            out.push(TABLE[(n >> 12) as usize & 63] as char);
            if chunk.len() > 1 {
                out.push(TABLE[(n >> 6) as usize & 63] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(TABLE[n as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// 造一张渐变 PNG，给定边长。
    fn make_png(size: u32) -> Vec<u8> {
        let mut img = image::RgbImage::new(size, size);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([
                (x * 255 / size.max(1)) as u8,
                (y * 255 / size.max(1)) as u8,
                128,
            ]);
        }
        let mut cur = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut cur, image::ImageFormat::Png)
            .unwrap();
        cur.into_inner()
    }

    /// 一张 1x1 PNG，用来确认最小的候选也能被接受。
    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    fn thumb_block(label: &str, bytes: &[u8]) -> String {
        format!(
            "; thumbnail begin {label} 0\n; {}\n; thumbnail end\n",
            base64_encode::encode(bytes)
        )
    }

    #[test]
    fn picks_the_largest_thumbnail_not_the_first() {
        // Orca 的真实排布：小的写前面，大的写后面。按「第一个」取会拿到 48x48。
        let text = format!(
            "; HEADER_BLOCK_START\n{}\n;\n{}\n\nG28\nG1 X10 Y20\n",
            thumb_block("48x48", &make_png(48)),
            thumb_block("300x300", &make_png(300)),
        );
        let c = convert(&text, None);
        let t = c.info.thumbnail.expect("should find a thumbnail");
        assert_eq!((t.width, t.height), (300, 300));
        assert!(!t.png.is_empty());
        assert_eq!(&t.png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn downscales_oversized_and_converts_bmp() {
        // 大图：裁到上限
        let text = thumb_block("600x600", &make_png(600));
        let t = pick_best_thumbnail(&text).expect("thumb");
        assert!(t.width <= THUMB_MAX && t.height <= THUMB_MAX);
        assert_eq!((t.width, t.height), (300, 300));

        // BMP 也能读进来并转成 PNG（Orca 可以把缩略图格式设成 BMP）
        let mut cur = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::new(64, 64))
            .write_to(&mut cur, image::ImageFormat::Bmp)
            .unwrap();
        let bmp = cur.into_inner();
        assert_eq!(&bmp[..2], b"BM");
        let t = pick_best_thumbnail(&thumb_block("64x64", &bmp)).expect("bmp thumb");
        assert_eq!(&t.png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!((t.width, t.height), (64, 64));
    }

    #[test]
    fn accepts_inline_and_tiny_thumbnails() {
        let text = format!(";thumbnail: data:image/png;base64,{TINY_PNG_B64}\n\nG28\n");
        let t = pick_best_thumbnail(&text).expect("inline thumb");
        assert_eq!((t.width, t.height), (1, 1));
    }

    #[test]
    fn skips_undecodable_candidates() {
        // 声明 300x300 但内容是垃圾 → 淘汰；有效的那张虽小仍然胜出。
        let text = format!(
            "; thumbnail begin 300x300 0\n; {}\n; thumbnail end\n{}",
            base64_encode::encode(b"not an image at all"),
            thumb_block("32x32", &make_png(32)),
        );
        let t = pick_best_thumbnail(&text).expect("valid thumb");
        assert_eq!((t.width, t.height), (32, 32));
    }

    #[test]
    fn parses_duration_formats() {
        assert_eq!(parse_duration("2h 14m 6s"), Some(8046.0));
        assert_eq!(parse_duration("26s"), Some(26.0));
        assert_eq!(parse_duration("7300"), Some(7300.0));
        assert_eq!(parse_duration("1h2m3s"), Some(3723.0));
    }

    #[test]
    fn parses_spaced_orca_keys() {
        let text = "; layer_height = 0.25\n\
                    ; filament used [mm] = 16143.76\n\
                    ; total filament used [g] = 49.31\n\
                    ; filament_type = PETG\n\
                    ; hot_plate_temp_initial_layer = 80\n\
                    ; nozzle_temperature_initial_layer = 255\n\
                    ; estimated printing time (normal mode) = 2h 14m 6s\n\
                    ; MINX:0\n; MINY:0\n; MINZ:0.2\n; MAXX:100\n; MAXY:120\n; MAXZ:20\n\
                    \nM109 S255\nM190 S80\nG1 X5 Y5\nG1 X95 Y115\n";
        let c = convert(text, None);
        assert_eq!(c.info.layer_height, 0.25);
        assert!((c.info.length_m - 16.14376).abs() < 1e-6);
        assert_eq!(c.info.weight_g, 49.31);
        assert_eq!(c.info.material, "PETG");
        assert_eq!(c.info.nozzle_temp, 255.0);
        assert_eq!(c.info.bed_temp, 80.0);
        assert_eq!(c.info.time_s, 8046.0);
        assert_eq!(c.info.bounds.max_x, 100.0);
        // 头部必须完整、结尾是正文
        assert!(c.output.starts_with(";Header Start\n"));
        assert!(c.output.contains(";machine: Starfish A6"));
        assert!(c.output.contains(";TIME:8046"));
        assert!(c.output.contains(";matierial_weight: 49.31"));
        assert!(c.output.contains(";nozzle_0_material: PETG"));
        assert!(c.output.trim_end().ends_with("G1 X95 Y115"));
    }

    #[test]
    fn b64_roundtrip() {
        let raw: Vec<u8> = (0u8..=255).collect();
        let enc = base64_encode::encode(&raw);
        assert_eq!(b64_decode(&enc).unwrap(), raw);
        // 带换行/空格也要能解
        let spaced = enc
            .as_bytes()
            .chunks(40)
            .map(|c| c.iter().map(|&b| b as char).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n ");
        assert_eq!(b64_decode(&spaced).unwrap(), raw);
    }

    #[test]
    fn idempotent_on_own_output() {
        let text = ";TIME:100\n;nozzle_temperature(°C): 210\n\nG28\nG1 X1 Y1\n";
        let first = convert(text, None);
        let second = convert(&first.output, None);
        assert_eq!(first.output, second.output);
        assert_eq!(second.info.nozzle_temp, 210.0);
    }

    /// 拿真实切片文件验一遍解析：`set SF_GCODE=xxx.gcode && cargo test -- --ignored real_gcode`
    #[test]
    #[ignore = "需要 SF_GCODE 指向一个真实切片文件"]
    fn real_gcode() {
        let path = std::env::var("SF_GCODE").expect("set SF_GCODE");
        let text = std::fs::read_to_string(&path).expect("read gcode");
        let started = std::time::Instant::now();
        let c = convert(&text, None);
        println!("file      : {path}");
        println!("elapsed   : {:?}", started.elapsed());
        println!("summary   : {}", c.info.summary());
        println!("time_s    : {:.1}", c.info.time_s);
        println!("layer_h   : {}", c.info.layer_height);
        println!("temps     : nozzle {:.0} / bed {:.0}", c.info.nozzle_temp, c.info.bed_temp);
        println!("material  : {}", c.info.material);
        println!("length_m  : {:.3}", c.info.length_m);
        println!("weight_g  : {:.2}", c.info.weight_g);
        println!("bounds    : {:?}", c.info.bounds);
        println!("line_count: {}", c.info.line_count);
        match &c.info.thumbnail {
            Some(t) => println!("thumbnail : {}x{} {} bytes from {}", t.width, t.height, t.png.len(), t.source),
            None => println!("thumbnail : none"),
        }
        println!("--- first 20 header lines ---");
        for line in c.output.lines().take(20) {
            let shown: String = line.chars().take(100).collect();
            println!("{shown}");
        }
    }
}
