//! starfish-control 入口。
//!
//! 模块划分：
//! - `protocol`：starfish-net 协议层（纯 Rust，UI/CLI 共用，不依赖 Makepad）
//! - `ipc`：单实例本地服务（命令行 ↔ 图形界面交接）
//! - `handoff`：命令行后处理模式（Orca 的后处理脚本走这里，做完就退出）
//! - `naming`：从切片器的临时路径反推真实导出文件名（界面上才不会显示 `.11596.1.gcode.pp`）
//! - `raise`：命令行交接时把窗口抬到前台（抬不动就闪任务栏，不强抢焦点）
//! - `log`：落盘运行日志；目前先采集后处理进程的环境变量快照
//! - `cli`：命令行参数解析
//! - `app`：Makepad 2.0 应用（Splash UI + Elm 状态 + 异步桥）
//! - `fonts`：把界面字体内嵌进 exe（换机器/单文件分发时不会变成方块）
//! - `eta`：打印剩余时间估算（协议不上报，只能自己算）

mod app;
mod cli;
mod eta;
mod fonts;
mod handoff;
mod ipc;
mod log;
mod naming;
mod protocol;
mod raise;

use makepad_widgets::*;

use crate::app::App;

// 命令行后处理模式必须在建窗口之前拦下来：Orca 是同步等这个进程的，
// 它多活几秒（更别说开个窗口等人点「开始打印」），切片器就一直转圈。
// `configure:` 是 Makepad 在 `Event::Startup` 里、`script_mod` 之前调的第一个钩子，
// 那时窗口还没建，退出是干净的。
app_main!(App, configure: |_cx: &mut Cx| {
    // `--dump-env`：只抓一份环境快照就退出，不做后处理、不交接、不开窗口、不碰设备。
    // 用来验证日志链路本身，或从任意父进程手动采样（对比哪些变量是切片器加的）。
    if std::env::args_os()
        .skip(1)
        .any(|a| a.to_string_lossy() == "--dump-env")
    {
        let _ = crate::log::capture_postprocess("<手动采样 --dump-env>");
        std::process::exit(0);
    }
    if let Some(code) = crate::handoff::run_if_postprocess() {
        std::process::exit(code);
    }
});
