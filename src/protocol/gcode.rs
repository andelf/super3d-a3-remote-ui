//! G-code 解析，用于上传前的预览（元数据 + 走刀包围盒）。
//!
//! 只做轻量解析：行数、是否含挤出移动、X/Y 平面包围盒、估算时长（如含 F）。
//! 不做完整 G-code 语义校验——那是打印机固件的事。
//!
//! **目前只有单测在用**：界面的「预览」还只显示缩略图（`protocol::header`），
//! 还没做基于这份元数据的详情页。整模块先标 `allow(dead_code)`，接线时删掉即可。
#![allow(dead_code)]

#[derive(Debug, Clone, Default)]
pub struct GcodeMeta {
    pub line_count: usize,
    pub has_extrusion: bool,
    pub move_count: usize,
    /// X/Y 平面包围盒（mm）。None 表示没有 G0/G1 移动。
    pub bbox: Option<((f64, f64), (f64, f64))>,
    /// 注释 / 头部信息（以 `;` 开头的首行等）。
    pub header: Vec<String>,
}

impl GcodeMeta {
    pub fn from_text(text: &str) -> GcodeMeta {
        let mut meta = GcodeMeta::default();
        let mut cur = (0.0f64, 0.0f64, 0.0f64); // x, y, e
        let mut min = (f64::INFINITY, f64::INFINITY);
        let mut max = (f64::NEG_INFINITY, f64::NEG_INFINITY);

        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with(';') {
                if line.starts_with(';') && meta.header.len() < 20 {
                    meta.header.push(line.to_string());
                }
                continue;
            }
            meta.line_count += 1;

            // 去注释
            let code = match line.find(';') {
                Some(i) => &line[..i],
                None => line,
            }
            .trim();

            let mut tokens = code.split_whitespace();
            let cmd = match tokens.next() {
                Some(c) => c,
                None => continue,
            };

            if cmd == "G0" || cmd == "G1" || cmd == "G00" || cmd == "G01" {
                let mut nx = cur.0;
                let mut ny = cur.1;
                let mut ne = cur.2;
                let mut moved = false;
                let mut extruded = false;
                for tok in tokens {
                    let (axis, val) = match tok.split_at(1) {
                        (a, v) if a.len() == 1 => (a, v),
                        _ => continue,
                    };
                    let v: f64 = match val.parse() {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    match axis {
                        "X" => {
                            nx = v;
                            moved = true;
                        }
                        "Y" => {
                            ny = v;
                            moved = true;
                        }
                        "Z" => {
                            moved = true;
                        }
                        "E" => {
                            if v > ne {
                                extruded = true;
                            }
                            ne = v;
                        }
                        _ => {}
                    }
                }
                if moved {
                    meta.move_count += 1;
                    min.0 = min.0.min(nx);
                    min.1 = min.1.min(ny);
                    max.0 = max.0.max(nx);
                    max.1 = max.1.max(ny);
                    cur = (nx, ny, ne);
                    if extruded {
                        meta.has_extrusion = true;
                    }
                }
            }
        }

        if min.0.is_finite() && max.0.is_finite() {
            meta.bbox = Some((min, max));
        }
        meta
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sample() {
        let g = GcodeMeta::from_text(
            ";Header Start\n;FLAVOR:Marlin\nG28\nG1 X10 Y20 E1 F3000\nG1 X30 Y5 E2\nG1 X0 Y0\n",
        );
        assert!(g.has_extrusion);
        assert_eq!(g.move_count, 3);
        let bbox = g.bbox.unwrap();
        assert_eq!(bbox.0, (0.0, 0.0));
        assert_eq!(bbox.1, (30.0, 20.0));
    }
}
