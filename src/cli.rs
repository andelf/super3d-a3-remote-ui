//! 命令行参数解析。
//!
//! ```text
//! starfish-control.exe [选项] [<file.gcode>]
//!
//!   <file.gcode>            位置参数：转换头部 → 自动连接第一台打印机 → 自动上传一次
//!                           → 把刚上传的文件置顶 → 询问是否开始打印
//!   --rewrite               顺便把转换后的头部写回 <file.gcode>（保留 .orca-original 备份）
//!   --material <文本>       写进 ;nozzle_0_material: 的材料名，如 黑色-PETG
//!   --host 192.168.0.227    指定目标；缺省=自动发现并连接第一台
//!   --list                  连接、列出机内文件、输出到 stdout、退出（无窗口）
//!   --status                连接、打印状态、退出（无窗口）
//!   --yes                   跳过「是否开始打印」的询问，直接开打（默认关闭，安全）
//! ```
//!
//! 作为 OrcaSlicer 的后处理脚本时，Orca 会用 `<脚本> <gcode路径>` 调用我们，
//! 正好命中位置参数这一条路径。

#[derive(Debug, Clone, Default)]
pub struct Cli {
    /// 位置参数：要后处理并上传的 G-code。
    pub file: Option<String>,
    /// 兼容旧写法 `--upload <path>`。
    pub upload: Option<String>,
    /// 把转换结果写回原文件（保留 .orca-original 备份）。
    pub rewrite: bool,
    /// 材料名覆盖。
    pub material: Option<String>,
    /// 列出设备文件。
    pub list: bool,
    /// 打印设备状态。
    pub status: bool,
    /// 指定目标主机（缺省则自动发现）。
    pub host: Option<String>,
    /// 跳过「是否开始打印」的询问。
    pub yes: bool,
    /// 删除机内文件（`--delete <名字>`，可重复）。名字必须写全，不做通配。
    pub delete: Vec<String>,
}

impl Cli {
    /// 解析 `std::env::args()`。第一个元素是程序名。
    pub fn parse() -> Cli {
        let mut cli = Cli::default();
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--upload" => cli.upload = args.next(),
                "--rewrite" | "--write-back" => cli.rewrite = true,
                "--material" => cli.material = args.next(),
                "--list" => cli.list = true,
                "--status" => cli.status = true,
                "--host" => cli.host = args.next(),
                "--yes" | "-y" => cli.yes = true,
                "--delete" => {
                    // 只认完整名字：不做通配、不批量删，删错就是不可恢复。
                    match args.next() {
                        Some(name) => cli.delete.push(name),
                        None => eprintln!("--delete 需要一个文件名"),
                    }
                }
                // Makepad 自带的远程调试参数（`--remote addr:port`）：
                // 它由框架自己解析，我们只需把值一起跳过，
                // 否则那个 `addr:port` 会被当成位置参数（文件名）。
                "--remote" => {
                    let _ = args.next();
                }
                other => {
                    if other.starts_with('-') {
                        eprintln!("忽略未知参数: {other}");
                    } else if cli.file.is_none() {
                        // 位置参数：Orca 的后处理调用就长这样，静默接受。
                        cli.file = Some(other.to_string());
                    } else {
                        eprintln!("忽略多余参数: {other}");
                    }
                }
            }
        }
        cli
    }

    /// 要后处理 / 上传的文件（位置参数优先，其次旧的 `--upload`）。
    pub fn target_file(&self) -> Option<&str> {
        self.file.as_deref().or(self.upload.as_deref())
    }

    /// 是否进入 headless（无窗口）模式。
    pub fn is_headless(&self) -> bool {
        self.list || self.status || !self.delete.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_file_is_accepted() {
        let cli = Cli {
            file: Some("a.gcode".into()),
            ..Default::default()
        };
        assert_eq!(cli.target_file(), Some("a.gcode"));
        // 旧的 --upload 仍可用
        let cli = Cli {
            upload: Some("b.gcode".into()),
            ..Default::default()
        };
        assert_eq!(cli.target_file(), Some("b.gcode"));
    }
}
