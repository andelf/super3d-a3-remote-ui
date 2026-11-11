//! 命令行侧：OrcaSlicer 调过来的那个「后处理脚本」。
//!
//! 这个进程**不成窗口、不碰设备**：读完文件、改写头部、把「要上传」这件事交给
//! 常驻的图形界面，然后立刻退出。切片器是同步等这个进程的，早退早解放。
//!
//! 全流程：
//! 1. `std::fs::canonicalize` —— Orca 的工作目录和我们不一样，相对路径必须先钉成绝对路径。
//! 2. `header::convert` —— 纯本地字符串处理，几十毫秒。
//! 3. `--rewrite` 时写回原文件（留 `.orca-original` 备份）。
//! 4. 交给界面：已有实例就发过去；没有再拉起一个（不继承 stdout/stderr，
//!    否则管道一直不关，切片器照样等）。
//! 5. `std::process::exit`。
//!
//! 退出码只反映**后处理**成没成：交接失败不算失败（文件已经改好了，用户还能手动拖进界面），
//! 否则 Orca 会在导出结束时弹一个吓人的错误框。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cli::Cli;
use crate::ipc::{self, Request};
use crate::protocol::header;

/// 等一个刚拉起来的界面把本地服务挂上要多久。
///
/// 冷启动要装字体、建 d3d11 设备，实测 1–3 秒；给 15 秒是因为那台机器上可能
/// 还挂着 19 MB 的中文字面在解码。超过这个时间还没人接，就认输。
const WAIT_FOR_GUI: Duration = Duration::from_secs(15);
/// 等待期间的重试间隔。
const RETRY_EVERY: Duration = Duration::from_millis(120);

/// 如果本次进程是「后处理模式」，做完就返回退出码。
///
/// 返回 `None` = 不是后处理模式，继续按图形界面启动。
pub fn run_if_postprocess() -> Option<i32> {
    let cli = Cli::parse();
    // `--list` / `--status` / `--delete` 是给脚本用的设备查询，保留原来的 headless 路径。
    if cli.is_headless() {
        return None;
    }
    let path = cli.target_file()?.to_string();

    // 确定是后处理调用了，先把切片器塞进来的环境变量原样留一份。
    // 必须在任何 IO / 交接之前做：这是「脚本被调起来那一刻」的环境，
    // 后面不管成功失败都能查。写日志失败不影响后处理的退出码。
    let _ = crate::log::capture_postprocess(&path);

    Some(hand_off(cli, path))
}

fn hand_off(cli: Cli, raw_path: String) -> i32 {
    // ── 1. 路径钉成绝对路径 ──────────────────────────────────────────────
    let path: PathBuf = match std::fs::canonicalize(&raw_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[starfish] 打不开 {raw_path}：{e}");
            return 2;
        }
    };

    // ── 2. 后处理 ────────────────────────────────────────────────────────
    let raw = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[starfish] 读取 {} 失败：{e}", path.display());
            return 2;
        }
    };
    let text = String::from_utf8_lossy(&raw);
    let converted = header::convert(&text, cli.material.as_deref());
    if converted.output.trim().is_empty() {
        eprintln!("[starfish] {} 后处理后没有内容，已放弃", path.display());
        return 3;
    }

    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("upload.gcode")
        .to_string();

    // 切片器传的是它自己的临时文件（`Metadata\.11596.1.gcode.pp`），拿它当上传名
    // 界面上就是一串看不懂的点号数字。名字优先问切片器要：它在环境变量里直接
    // 给了真实导出文件的完整路径，比从内部目录反推准得多（实测多 plate 导出时
    // 会话目录里连 origin.txt 都没有，反推会整条失效）。
    // 注意：**只取名字，绝不读那个路径** —— 文件要等本进程退出后才由 Orca 写出。
    let from_env = crate::naming::env_export_name();
    if let Some(n) = &from_env {
        eprintln!(
            "[starfish] 导出名来自 {}：{n}（只取名字，不读那个文件）",
            crate::naming::OUTPUT_NAME_ENV
        );
    }
    let upload_name = resolve_upload_name(&path, &converted.info, &name, from_env);

    // ── 3. `--rewrite`：把结果写回原文件 ─────────────────────────────────
    if cli.rewrite {
        if let Err(e) = write_back(&path, &converted.output) {
            eprintln!("[starfish] 写回 {} 失败：{e}", path.display());
            return 4;
        }
    }

    if upload_name != name {
        eprintln!("[starfish] {name} → 上传名 {upload_name}");
    }
    eprintln!(
        "[starfish] 已后处理 {upload_name}：{}",
        converted.info.summary()
    );

    // ── 4. 交给界面 ──────────────────────────────────────────────────────
    // 交给界面的**不是切片器给的路径**，而是我们自己落地的副本。原因见
    // `stage_copy`：切片器在脚本退出后马上就会动那个文件。
    // 副本用真实导出名落地，界面照文件名给设备上的文件命名，用户一眼能认出来。
    let origin = display_path(&path);
    let staged = stage_copy(&upload_name, &converted.output);
    if staged.is_none() {
        eprintln!("[starfish] 交接副本落地失败（{}），界面只能直接读原路径", staging_root().display());
    }
    let req = Request {
        path: staged
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| origin.clone()),
        origin: Some(origin),
        material: cli.material.clone(),
        host: cli.host.clone(),
        start: cli.yes,
    };

    // 把「允许设置前台窗口」的许可转授出去。我们这个进程通常由切片器（当时的
    // 前台程序）拉起，有这份许可；界面进程是被动的后台进程，没有它
    // `SetForegroundWindow` 会被系统拒掉，只能退化成闪任务栏。
    crate::raise::allow_foreground();

    if ipc::is_running() {
        match ipc::send(&req) {
            Ok(msg) => {
                eprintln!("[starfish] 已交给正在运行的界面：{msg}（本进程退出）");
                return 0;
            }
            // 探活成功但交接失败（界面正好在退出）——继续往下走，试着拉起一个新的。
            Err(e) => eprintln!("[starfish] 界面在跑但没接住这次请求：{e}"),
        }
    } else {
        eprintln!("[starfish] 没有正在运行的界面，先拉起一个");
    }

    // 没人在听 → 拉一个起来。界面本身不带参数，避免它又把自己当后处理。
    if let Err(e) = spawn_gui() {
        eprintln!("[starfish] 启动界面失败：{e}（文件已后处理好，可手动拖进界面）");
        return 0;
    }

    // ── 5. 等它把服务挂起来，然后交接 ────────────────────────────────────
    let deadline = Instant::now() + WAIT_FOR_GUI;
    let mut tried = 0u32;
    while Instant::now() < deadline {
        std::thread::sleep(RETRY_EVERY);
        tried += 1;
        if let Ok(msg) = ipc::send(&req) {
            eprintln!(
                "[starfish] 已启动界面并交接给 {name}：{msg}（本进程退出，共重试 {tried} 次）"
            );
            return 0;
        }
    }

    // 后处理是成功的，交接失败只警告不报错：不能让 Orca 因为「界面没起来」把导出判失败。
    eprintln!(
        "[starfish] {upload_name} 已后处理好，但界面在 {} 秒内没接受任务；\
         文件可手动拖进 starfish-control 上传",
        WAIT_FOR_GUI.as_secs()
    );
    // 没人接，副本就没用了（原文件还在，用户要传的是它）。
    if let Some(dir) = staged.as_ref().and_then(|p| p.parent()) {
        let _ = std::fs::remove_dir_all(dir);
    }
    0
}

/// 上传名的四级来源，按可靠度排序。
///
/// 抽成纯函数是为了能单测：环境变量用参数注入，不去动进程全局环境
/// （单测是并行的，直接 `set_var` 会互相踩）。
fn resolve_upload_name(
    gcode: &Path,
    info: &header::HeaderInfo,
    fallback: &str,
    from_env: Option<String>,
) -> String {
    // 1 = from_env（切片器明说的真实导出名）；2 = 按 filename_format 模板反推；
    // 3 = 只剩 origin.txt 时给个「模型名.gcode」；4 = 全失败，至少别把 `.pp`
    // 和点前缀带上去。
    from_env
        .or_else(|| crate::naming::orca_export_name(gcode, info))
        .or_else(|| crate::naming::origin_based_name(gcode))
        .unwrap_or_else(|| strip_pp(fallback))
}

/// 去掉切片器临时文件的 `.pp` 后缀（`xxx.gcode.pp` → `xxx.gcode`）。
///
/// 只在算不出真实导出名时兜底：至少别让界面上出现 `.pp`。
/// 同时去掉点前缀——设备端把 `.` 开头的当隐藏文件，列表接口不列，传了也白传。
fn strip_pp(name: &str) -> String {
    let base = match name.strip_suffix(".pp") {
        Some(rest) if !rest.is_empty() => rest,
        _ => name,
    };
    base.trim_start_matches('.').to_string()
}

/// 交接副本的根目录（`%TEMP%\starfish-handoff`）。
///
/// 界面清理副本时也认这个前缀，所以两边必须用同一份定义。
pub fn staging_root() -> PathBuf {
    std::env::temp_dir().join("starfish-handoff")
}

/// 把后处理结果落到**自己的**临时目录里，把那个路径交给界面。
///
/// 为什么不直接让界面读切片器给的路径：Orca 是「导出结束」时同步跑后处理脚本，
/// 脚本一退出它就继续收尾——挪走文件、重开写句柄、清临时目录。界面是异步接活的，
/// 冷启动还要 1–3 秒，等它真去读时原路径可能已经不存在或被独占。
/// 副本的寿命由我们自己控制，读不到的风险就没了。
fn stage_copy(name: &str, output: &str) -> Option<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = staging_root().join(format!("{}-{}", std::process::id(), stamp));
    std::fs::create_dir_all(&dir).ok()?;
    // 文件名保持原样：界面就是按文件名给设备上的文件命名的。
    let path = dir.join(sanitize(name));
    std::fs::write(&path, output.as_bytes()).ok()?;
    Some(path)
}

/// 去掉文件名里不能落盘的部分（切片器的文件名已经很干净，这里是兜底）。
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    if cleaned.is_empty() {
        "upload.gcode".to_string()
    } else {
        cleaned
    }
}

/// 写回原文件，第一次写之前备份成 `<name>.orca-original`。
fn write_back(path: &Path, output: &str) -> std::io::Result<()> {
    let backup = path.with_extension(format!(
        "{}orca-original",
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| format!("{e}."))
            .unwrap_or_default()
    ));
    if !backup.exists() {
        std::fs::copy(path, &backup)?;
    }
    std::fs::write(path, output.as_bytes())
}

/// 拉起一个不带参数的自己。
///
/// **三个 stdio 必须置空**：Orca 是用管道读脚本输出的，只要管道还被别的进程占着，
/// 即使脚本进程已经退出，它也等不到 EOF —— 那就又卡回去了。
fn spawn_gui() -> std::io::Result<()> {
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS：别继承调用者的控制台（否则 Orca 的控制台会被这个
        // 界面一直占着）；CREATE_NEW_PROCESS_GROUP：从 Orca 的进程组里脱出来，
        // 它 Ctrl+C / 结束任务时不会顺带把界面打死。
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    cmd.spawn()?;
    Ok(())
}

/// 路径转字符串给 IPC 用。Windows 的 `canonicalize` 会加 `\\?\` 前缀，
/// 送去界面只为了读文件，留着无妨，但显示起来难看，这里去掉显示用的那份。
fn display_path(p: &Path) -> String {
    let s = p.display().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        // UNC 路径 `\\?\UNC\server\share` 不能直接砍前缀。
        if !rest.starts_with("UNC\\") {
            return rest.to_string();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_verbatim_prefix_for_display() {
        let p = Path::new(r"\\?\C:\Users\me\a.gcode");
        assert_eq!(display_path(p), r"C:\Users\me\a.gcode");
    }

    #[test]
    fn keeps_unc_verbatim_paths() {
        let p = Path::new(r"\\?\UNC\server\share\a.gcode");
        assert_eq!(display_path(p), r"\\?\UNC\server\share\a.gcode");
    }

    #[test]
    fn sanitize_keeps_normal_names() {
        assert_eq!(sanitize("Dragon_v2.gcode"), "Dragon_v2.gcode");
        assert_eq!(sanitize("切片 1.gcode"), "切片 1.gcode");
        assert_eq!(sanitize("a/b\\c.gcode"), "a_b_c.gcode");
        assert_eq!(sanitize(""), "upload.gcode");
    }

    #[test]
    fn stage_copy_writes_content_under_its_own_root() {
        let out = ";TIME:60\nG28\n";
        let p = stage_copy("切片 1.gcode", out).expect("应该能落地");
        assert!(p.starts_with(staging_root()), "副本必须在自己的根目录下：{p:?}");
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), "切片 1.gcode");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), out);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn write_back_makes_a_backup_once() {
        let dir = std::env::temp_dir().join(format!("starfish-wb-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("x.gcode");
        std::fs::write(&file, "original").unwrap();

        write_back(&file, "first").unwrap();
        write_back(&file, "second").unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "second");
        // 备份只在第一次写之前做，不能被第二次写覆盖成 "first"
        let backup = dir.join("x.gcode.orca-original");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "original");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fallback_name_drops_pp_and_the_hidden_dot() {
        // 兜底路径（算不出真实导出名）也要去掉点：点开头在设备上是隐藏文件
        assert_eq!(strip_pp(".11596.1.gcode.pp"), "11596.1.gcode");
        assert_eq!(strip_pp("a.gcode.pp"), "a.gcode");
        assert_eq!(strip_pp("plain.gcode"), "plain.gcode");
    }

    #[test]
    fn env_name_wins_and_template_is_the_fallback() {
        let dir = std::env::temp_dir().join(format!("starfish-handoff-{}-name", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Metadata")).unwrap();
        std::fs::write(dir.join("origin.txt"), r"C:\tmp\Part 1 (16).3mf").unwrap();
        std::fs::write(
            dir.join("_temp_1.config"),
            r#"{"filename_format": "{input_filename_base}_{filament_type[initial_tool]}_{print_time}.gcode", "filament_type": ["PETG"]}"#,
        )
        .unwrap();
        let gcode = dir.join("Metadata").join(".11596.1.gcode.pp");
        let info = header::HeaderInfo {
            time_s: 2611.0,
            ..Default::default()
        };

        // 有环境变量：直接用，哪怕反推也能算出个名字
        assert_eq!(
            resolve_upload_name(
                &gcode,
                &info,
                ".11596.1.gcode.pp",
                Some("Clip S1_PETG_43m31s.gcode".into())
            ),
            "Clip S1_PETG_43m31s.gcode"
        );
        // 没有环境变量：退回模板反推
        assert_eq!(
            resolve_upload_name(&gcode, &info, ".11596.1.gcode.pp", None),
            "Part 1 (16)_PETG_43m31s.gcode"
        );
        // 会话目录也不存在：最后兜底，且不带点前缀
        let orphan = Path::new("C:/nowhere/.11596.2.gcode.pp");
        assert_eq!(
            resolve_upload_name(orphan, &info, ".11596.2.gcode.pp", None),
            "11596.2.gcode"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
