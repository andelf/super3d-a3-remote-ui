//! 应用入口与 UI（Makepad 2.0 + Splash）。
//!
//! # 事件模型（关键，踩过坑）
//! - Splash 的 `on_click` 闭包由 **Splash 脚本 VM** 执行，只能写 `mod.state.*`
//!   或调 widget 方法，**不能**调用 Rust 自由函数（会报 `variable ... not found`）。
//! - 因此 UI 里 **不写任何回调**；所有交互统一在 Rust 的 `handle_actions` 里
//!   用 `self.ui.button(cx, ids!(..)).clicked(actions)` 分发。
//!
//! # 渲染模型（关键，踩过坑）
//! - **不要**在 `on_render` 里用 `for` / `if` 现搭 widget 树：每次重绘都会重建，
//!   导致行内容丢失、点击错乱（之前的「选中后行变空」就是这么来的）。
//! - 正确做法：widget 树是**静态**的，只建一次；状态变化用 Rust 侧 setter
//!   （`set_text` / `set_value` / `set_visible`）推给已有 widget。
//! - 列表用**固定行池**（设备 3 行、文件 8 行），空行 `set_visible(false)` 隐藏。
//!
//! # 架构
//! 协议层 `Connection` 放 `#[rust]` 容器；后台线程收包 → 主线程 `handle_timer`
//! 里非阻塞 drain → 解析成 `DeviceStatus` → `sync()` 刷 UI。

use makepad_widgets::makepad_platform::file_dialogs::{FileDialog, FileDialogAction};
use makepad_widgets::*;

use crate::cli::Cli;
use crate::eta::Eta;
use crate::ipc;
use crate::naming;
use crate::protocol::connection::{Connection, Device};
use crate::protocol::header::{self, fmt_duration, HeaderInfo};
use crate::protocol::status::{DeviceStatus, PrintStatus};

/// 设备行池大小。
const DEV_ROWS: usize = 3;
/// 文件行池大小。
const FILE_ROWS: usize = 8;
/// 上传进度条的段数。
///
/// Makepad 没有 ProgressBar，也没有运行时的 `set_width`，
/// 所以进度条 = 底轨 + 20 段等宽实心块，按百分比 `set_visible`。
const SEG_COUNT: usize = 20;
/// 可选步长（mm）。上限 10mm，避免误点造成长距离运动。
const STEPS: [f64; 3] = [0.1, 1.0, 10.0];
/// 上传文件选择框的 id：用它把回包和别的对话框区分开。
const PICK_GCODE: LiveId = live_id!(pick_gcode);

/// 有交接文件在等、但设备没连上时，隔多久重试一次连接。
///
/// 设备可能还没开机。低频重试是为了「开机后自动接着传」，不是为了刷屏——
/// 每次连接都在后台线程跑，主线程一次都不阻塞。
const CONNECT_RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// 后台连接线程送回主线程的结果。
///
/// `Connection` / `DeviceStatus` 都是纯数据 + channel，可以跨线程搬。
/// 初次文件列表也在这里一起拉完：它最长要等约 4.7 秒（分页 + 心跳），
/// 留在主线程做的话「刚连上」那一下照样会卡住窗口。
struct ConnectOutcome {
    ip: String,
    serial: String,
    /// Ok = (连接, 机内文件名, 握手时顺手读到的状态)
    result: std::io::Result<(Connection, Vec<String>, Option<DeviceStatus>)>,
}

/// 队列里有文件在等、设备没连上时，该拿哪台设备去重试。
///
/// 命令行点名的优先，其次第一台已知设备；一台都没有就返回 `None`（调用方去扫描）。
/// 抽成纯函数是因为 App 是 Makepad 的 Live 结构，单测里造不出实例。
fn pending_connect_target(queue: &[ipc::Request], devices: &[Device]) -> Option<String> {
    // 没有待传文件就没什么好等的，别凭空发起一次连接占设备会话。
    if queue.is_empty() {
        return None;
    }
    queue
        .iter()
        .find_map(|r| r.host.clone().filter(|h| !h.is_empty()))
        .or_else(|| devices.first().map(|d| d.ip.clone()))
}

/// 上传吞吐的实测值（官方 App 抓包 + 本程序实测都在 0.7–0.8 MB/s）。
///
/// 曾经的「3 MB 上限」是假象：真正的限制是设备 **4 秒会话看门狗**（见
/// `Connection::beat`），不发 8/0 时 4.01 秒必被断链，而 4 秒 × 0.78 MB/s ≈ 3.1 MB。
/// 修好心跳后大小不再受限，所以这里只用来给用户一个耗时预估。
const UPLOAD_RATE_BPS: f64 = 780.0 * 1024.0;

/// 拉一次机内文件列表，顺带把路上的 8/0 状态帧喂给 `status`。
///
/// 设备是**分页**的：`10/1` 只是 ACK，收到它之后要再发 `10/2` 才真的下发一页；
/// 每页回包 `[4..8]` 是名字块长度，名字从 `[8..8+n]` 开始、NUL 分隔。
fn fetch_file_list(
    conn: &Connection,
    status: &mut Option<DeviceStatus>,
    dump: bool,
) -> Vec<String> {
    use std::time::{Duration, Instant};

    // ① 先握手：clientId 只在设备回包里下发，连接后立刻发 10/1 会被拒。
    let _ = conn.status_query();
    let deadline = Instant::now() + Duration::from_millis(600);
    while Instant::now() < deadline {
        conn.beat();
        match conn.recv(Duration::from_millis(150)) {
            Some(p) => {
                if p.len() >= 39 && p[0] == 8 && p[1] == 0 {
                    if let Some(st) = DeviceStatus::from_payload(&p) {
                        *status = Some(st);
                    }
                    break;
                }
            }
            None => break,
        }
    }

    // ② 分页拉取（最多两次尝试）。
    //
    // ⚠️ 这里也必须打心跳：两轮加起来最长 ≈ 0.6 + 2×(1.8+0.25) ≈ 4.7 秒，
    // 而设备的会话看门狗是 4.01 秒（见 `Connection::beat`）。少了这一句，
    // 「上传完成 → 刷新列表」就会在最后一步把连接等死，表现成列表变空。
    for _attempt in 0..2 {
        let mut blob: Vec<u8> = Vec::new();
        let _ = conn.file_list_begin(0);

        let mut state = 0u8;
        let deadline = Instant::now() + Duration::from_millis(1800);
        while Instant::now() < deadline {
            conn.beat();
            let p = match conn.recv(Duration::from_millis(200)) {
                Some(p) => p,
                None => {
                    if state >= 2 {
                        break;
                    }
                    continue;
                }
            };

            if p.len() >= 39 && p[0] == 8 && p[1] == 0 {
                if let Some(st) = DeviceStatus::from_payload(&p) {
                    *status = Some(st);
                }
                continue;
            }

            if p.len() > 4 && p[0] == 10 {
                if dump {
                    let hex: Vec<String> = p.iter().take(48).map(|b| format!("{b:02x}")).collect();
                    eprintln!("[list] 10/{} len={} {}", p[1], p.len(), hex.join(" "));
                }
                match p[1] {
                    1 => {
                        let _ = conn.file_list_page();
                        state = 1;
                    }
                    2 => {
                        if p.len() >= 8 {
                            let n = u32::from_le_bytes([p[4], p[5], p[6], p[7]]) as usize;
                            let end = (8 + n).min(p.len());
                            blob.extend_from_slice(&p[8..end]);
                            let _ = conn.file_list_page();
                            let _ = conn.file_list_end();
                            state = 2;
                        }
                    }
                    3 => {
                        break;
                    }
                    _ => {}
                }
            }
        }

        let names: Vec<String> = blob
            .split(|&b| b == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !names.is_empty() {
            return names;
        }
        // 空结果：同步一次会话再试。
        let _ = conn.status_query();
        let _ = conn.recv(Duration::from_millis(250));
    }
    Vec::new()
}

// ───────────────────────────── 意图（Rust 侧动作） ─────────────────────────────

#[derive(Debug, Clone)]
enum Intent {
    /// 连接第 idx 台已发现设备。
    Connect(usize),
    /// 打印机内文件。
    Print(String),
    /// 删除机内文件。
    Delete(String),
    /// 回原点（整机运动，较长距离）。
    Home,
    /// 上传本地 G-code 到设备并打印。
    UploadPrint(String),
    /// 只上传（命令行带文件启动时用）：上传完再问要不要打印。
    UploadOnly(String),
    /// 应用温度 / 风扇设定值。
    ApplyEnv,
    /// 单轴点动：axis 0=X/1=Y/2=Z，dir ±1。
    Jog(u8, i32),
    /// 挤出（正）或回抽（负），单位 1/1000 mm。
    Extrude(i32),
    /// 释放步进电机（之后可用手推动喷头）。
    MotorOff,
    /// 断开当前连接。
    Disconnect,
}

impl Intent {
    /// 会带来较大机械动作、需要二次确认的意图。
    fn is_dangerous(&self) -> bool {
        matches!(
            self,
            Intent::Print(_)
                | Intent::Delete(_)
                | Intent::Home
                | Intent::UploadPrint(_)
                | Intent::Extrude(_)
                | Intent::MotorOff
        )
    }

    /// 会往设备写东西（存储或机械动作）的意图。
    ///
    /// 上传期间这些操作必须挡住：`wait_ack(9,2)` 会把别的请求的应答当无关帧丢掉，
    /// 插进来的 print/delete 既可能不生效，也可能打乱上传的分片节奏。
    fn writes_device(&self) -> bool {
        matches!(
            self,
            Intent::Print(_)
                | Intent::Delete(_)
                | Intent::Home
                | Intent::UploadPrint(_)
                | Intent::UploadOnly(_)
                | Intent::ApplyEnv
                | Intent::Jog(..)
                | Intent::Extrude(_)
                | Intent::MotorOff
        )
    }

    /// 确认弹窗里的中文说明。
    fn describe(&self) -> String {
        match self {
            Intent::Print(n) => format!("开始打印：{n}\n喷头会移动到起点并开始挤出。"),
            Intent::Delete(n) => format!(
                "删除机内文件：{n}\n删除后不可恢复。\n\
                 注意：机内删除的文件名解析还没验证（实测出现过「删一个掉好几个」），\
                 操作前请先刷新文件列表确认。"
            ),
            Intent::Home => "回原点 Home\n三轴会依次回零，属于较长距离运动。".to_string(),
            Intent::UploadPrint(p) => {
                format!("上传并打印：{p}\n文件会先写入机内存储，然后立即开始打印。")
            }
            Intent::UploadOnly(p) => format!("上传：{p}"),
            Intent::Extrude(um) => format!(
                "{} {:.1} mm\n需喷头已达工作温度，否则可能堵料。",
                if *um > 0 { "挤出" } else { "回抽" },
                (*um as f64 / 1000.0).abs()
            ),
            Intent::MotorOff => "释放电机\n步进电机会失力，喷头/平台可能因重力下沉。".to_string(),
            Intent::ApplyEnv => "应用温度与风扇设定".to_string(),
            Intent::Connect(_) => "连接设备".to_string(),
            Intent::Disconnect => "断开连接".to_string(),
            Intent::Jog(a, d) => format!("点动 axis{a} dir{d}"),
        }
    }
}

/// 顶栏状态灯的四种外观。
///
/// Makepad 没有运行时改配色的接口，所以「一种状态一个胶囊」，靠可见性切换：
/// 灰=未连接，蓝=已连接空闲，绿=正在干活，琥珀=需要人看一眼。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateKind {
    Off,
    Idle,
    Run,
    Warn,
}

impl StateKind {
    fn of(connected: bool, s: PrintStatus) -> StateKind {
        if !connected {
            return StateKind::Off;
        }
        match s {
            PrintStatus::Free => StateKind::Idle,
            // 正在动：打印、收尾、恢复、回零、调平、Z 补偿…
            PrintStatus::Printing
            | PrintStatus::Processing
            | PrintStatus::EndingForPrint
            | PrintStatus::PauseResume
            | PrintStatus::PowerLossRecovery
            | PrintStatus::Homing
            | PrintStatus::AutoLeveling
            | PrintStatus::ManualLeveling
            | PrintStatus::ZOffsetMoving => StateKind::Run,
            // 停下来等人：暂停、异常、缺料、读文件异常、关机、未知。
            _ => StateKind::Warn,
        }
    }
}

fn zh_state(s: PrintStatus) -> &'static str {
    match s {
        PrintStatus::Free => "空闲",
        PrintStatus::Printing => "打印中",
        PrintStatus::Pause => "已暂停",
        PrintStatus::Pausing => "暂停中",
        PrintStatus::ErrorPause => "异常暂停",
        PrintStatus::ErrorPausing => "异常暂停中",
        PrintStatus::EndingForPrint => "收尾中",
        PrintStatus::PauseResume => "恢复中",
        PrintStatus::PowerLossRecovery => "断电续打",
        PrintStatus::Processing => "处理中",
        PrintStatus::FilamentPause => "缺料暂停",
        PrintStatus::FilamentPausing => "缺料暂停中",
        PrintStatus::ReadFileAbnormalPause => "读文件异常",
        PrintStatus::ReadFileAbnormalPausing => "读文件异常暂停",
        PrintStatus::AutoLeveling => "自动调平",
        PrintStatus::ManualLeveling => "手动调平",
        PrintStatus::Homing => "回零中",
        PrintStatus::ZOffsetMoving => "Z 补偿中",
        PrintStatus::PowerOff => "已关机",
        PrintStatus::Unknown(_) => "未知",
    }
}

// ───────────────────────────── Splash UI ─────────────────────────────

script_mod! {
    use mod.prelude.widgets.*

    // ── 样式基元 ──
    let SfCard = RoundedView {
        width: Fill
        height: Fit
        flow: Down
        spacing: 10
        padding: 14
        draw_bg +: {
            color: #x141a23
            border_radius: 12
            border_size: 1
            border_color: #x222c3a
        }
    }

    let SfCaption = Label {
        draw_text.text_style.font_size: 12
        draw_text.color: #x7f8fa3
    }

    let SfSection = Label {
        draw_text.text_style.font_size: 13
        draw_text.color: #x9db0c6
    }

    let SfValue = Label {
        draw_text.text_style.font_size: 14
        draw_text.color: #xe8eef7
    }

    let SfRow = RoundedView {
        width: Fill
        height: 46
        flow: Right
        spacing: 8
        padding: Inset{left: 10 right: 10 top: 0 bottom: 0}
        align: Align{y: 0.5}
        draw_bg +: {
            color: #x1a2130
            border_radius: 8
        }
    }

    // ── 按钮：统一冷色暗色系（覆盖默认暖色主题） ──
    let BtnGhost = ButtonFlat {
        draw_bg +: {
            color: #x1b2431
            color_hover: #x24303f
            color_down: #x131b25
            color_focus: #x1b2431
            color_disabled: #x151b23
            border_color: #x2b3746
            border_color_hover: #x3c4c60
            border_color_down: #x2b3746
            border_color_focus: #x4da3ff
            border_color_disabled: #x1e2732
            border_radius: 8
            layer_color: #x00000000
        }
        draw_text +: {
            color: #xcfdae8
            color_hover: #xffffff
            color_down: #xcfdae8
            color_focus: #xffffff
            color_disabled: #x5c6b7d
        }
        draw_icon +: {
            color: #xcfdae8
        }
    }

    let BtnAccent = BtnGhost {
        draw_bg +: {
            color: #x2f6cf6
            color_hover: #x3f7bff
            color_down: #x2456c9
            color_focus: #x2f6cf6
            border_color: #x2f6cf6
            border_color_hover: #x3f7bff
            border_color_down: #x2456c9
            border_color_focus: #x9ec4ff
            layer_color: #x00000000
        }
        draw_text +: {
            color: #xffffff
            color_hover: #xffffff
            color_down: #xe6eeff
            color_focus: #xffffff
        }
        draw_icon +: {
            color: #xffffff
        }
    }

    let BtnDanger = BtnGhost {
        draw_bg +: {
            color: #xd92d3f
            color_hover: #xec3b4e
            color_down: #xb32334
            color_focus: #xd92d3f
            border_color: #xd92d3f
            border_color_hover: #xec3b4e
            border_color_down: #xb32334
            border_color_focus: #xff8b98
            layer_color: #x00000000
        }
        draw_text +: {
            color: #xffffff
            color_hover: #xffffff
            color_down: #xffdfe3
            color_focus: #xffffff
        }
        draw_icon +: {
            color: #xffffff
        }
    }

    // 开关：胶囊样式 + 蓝色激活
    let SfToggle = ToggleFlat {
        draw_bg +: {
            color_active: #x2f6cf6
            color_active_hover: #x3f7bff
            mark_color_active: #xffffff
            mark_color_active_hover: #xffffff
            mark_color: #x61708a
            outline_size: 1.0
            outline_color: #x2b3746
        }
        draw_text +: {
            color: #xcfdae8
            color_hover: #xffffff
        }
    }

    // 拖放上传区样式。
    //
    // ⚠️ 不要给 Dropzone 写 `draw_bg +: { ... }`（或 draw_text/draw_hint 的颜色）：
    // 它的 draw 带 Rust 实例字段（accepting/refusing/hover/focus/disabled），
    // 任何上层 override 都会重建值表、把这些字段变成 void，shader 编译直接失败：
    //   draw shader 'DrawDropzone' failed to compile and will NOT be drawn
    //   sdf.rs:411 - shader builtin min requires matching float/int types, got void and f32
    // 实测在实例上和 `set_type_default()` 上都一样会炸。所以只设布局与文案，
    // 配色沿用主题（正好也保住了拖入时的 accepting/refusing 状态色）。
    mod.widgets.SfDrop = set_type_default() do mod.widgets.Dropzone {
        width: Fill
        height: 86
        margin: 0
        text: "拖入 .gcode 文件"
        hint: "或点击从本地选择"
        accept: "gcode"
        multiple: false
        show_filter_hint: false
        line_gap: 4.0
    }

    startup() do #(App::script_component(vm)) {
        ui: Root {
            main_window := Window {
                // 不写这行，标题栏会显示 Makepad 的默认标题（"Makepad"），
                // 跟本程序没关系；Windows 还会拿它当任务栏/Alt-Tab 的名字。
                window.title: "Super3D A3 远程打印控制"
                window.inner_size: vec2(1200, 820)
                body +: {
                    root := RoundedView {
                        width: Fill
                        height: Fill
                        flow: Down
                        draw_bg +: { color: #x0a0e13 }

                        // ══════════ 顶栏 ══════════
                        topbar := View {
                            width: Fill
                            height: 58
                            flow: Right
                            spacing: 12
                            padding: Inset{left: 18 right: 18 top: 0 bottom: 0}
                            align: Align{y: 0.5}

                            Label {
                                text: "STARFISH"
                                draw_text.text_style.font_size: 15
                                draw_text.color: #x4da3ff
                            }
                            host_label := Label {
                                text: "未连接设备"
                                width: Fill
                                draw_text.text_style.font_size: 13
                                draw_text.color: #x7f8fa3
                            }
                            // 连接/打印状态灯：右上唯一的连接状态来源。
                            // 断开连接的动作在左上「设备」行里（连着的那台按钮就是「断开」），
                            // 顶栏只留状态，所以这里是 4 个固定胶囊 + set_visible 切换——
                            // Makepad 没有运行时改配色的接口，只能一种状态一个胶囊。
                            pill_off := RoundedView {
                                width: Fit
                                height: 28
                                flow: Right
                                spacing: 7
                                padding: Inset{left: 11 right: 12 top: 0 bottom: 0}
                                align: Align{y: 0.5}
                                draw_bg +: {
                                    color: #x1b2431
                                    border_radius: 14
                                    border_size: 1
                                    border_color: #x28323f
                                }
                                RoundedView {
                                    width: 7
                                    height: 7
                                    draw_bg +: { color: #x55627a border_radius: 3.5 border_size: 0 }
                                }
                                pill_off_text := Label {
                                    text: "未连接"
                                    draw_text.text_style.font_size: 12.5
                                    draw_text.color: #x7f8fa3
                                }
                            }
                            pill_idle := RoundedView {
                                width: Fit
                                height: 28
                                flow: Right
                                spacing: 7
                                padding: Inset{left: 11 right: 12 top: 0 bottom: 0}
                                align: Align{y: 0.5}
                                draw_bg +: {
                                    color: #x132234
                                    border_radius: 14
                                    border_size: 1
                                    border_color: #x24405c
                                }
                                RoundedView {
                                    width: 7
                                    height: 7
                                    draw_bg +: { color: #x4da3ff border_radius: 3.5 border_size: 0 }
                                }
                                pill_idle_text := Label {
                                    text: "已连接"
                                    draw_text.text_style.font_size: 12.5
                                    draw_text.color: #xbcd3ea
                                }
                            }
                            pill_run := RoundedView {
                                width: Fit
                                height: 28
                                flow: Right
                                spacing: 7
                                padding: Inset{left: 11 right: 12 top: 0 bottom: 0}
                                align: Align{y: 0.5}
                                draw_bg +: {
                                    color: #x10251c
                                    border_radius: 14
                                    border_size: 1
                                    border_color: #x2a6b46
                                }
                                RoundedView {
                                    width: 7
                                    height: 7
                                    draw_bg +: { color: #x35d07f border_radius: 3.5 border_size: 0 }
                                }
                                pill_run_text := Label {
                                    text: "打印中"
                                    draw_text.text_style.font_size: 12.5
                                    draw_text.color: #x9df0bf
                                }
                            }
                            pill_warn := RoundedView {
                                width: Fit
                                height: 28
                                flow: Right
                                spacing: 7
                                padding: Inset{left: 11 right: 12 top: 0 bottom: 0}
                                align: Align{y: 0.5}
                                draw_bg +: {
                                    color: #x2a2110
                                    border_radius: 14
                                    border_size: 1
                                    border_color: #x6b5320
                                }
                                RoundedView {
                                    width: 7
                                    height: 7
                                    draw_bg +: { color: #xf0b429 border_radius: 3.5 border_size: 0 }
                                }
                                pill_warn_text := Label {
                                    text: "已暂停"
                                    draw_text.text_style.font_size: 12.5
                                    draw_text.color: #xffd98a
                                }
                            }
                            confirm_toggle := SfToggle {
                                text: "动作二次确认"
                                active: true
                                draw_text.text_style.font_size: 12.5
                            }
                        }

                        // 分隔线
                        View {
                            width: Fill
                            height: 1
                            show_bg: true
                            draw_bg +: { color: #x18202c }
                        }

                        // ══════════ 主体两列 ══════════
                        content := View {
                            width: Fill
                            height: Fill
                            flow: Right
                            spacing: 14
                            padding: Inset{left: 14 right: 14 top: 14 bottom: 14}

                            // ───────── 左列 ─────────
                            left_col := View {
                                width: 360
                                height: Fill
                                flow: Down
                                spacing: 12

                                // 设备卡
                                dev_card := SfCard {
                                    dev_head := View {
                                        width: Fill
                                        height: Fit
                                        flow: Right
                                        spacing: 8
                                        align: Align{y: 0.5}
                                        SfSection { text: "设备" width: Fill }
                                        rescan_btn := BtnGhost {
                                            text: "重新扫描"
                                            width: 92
                                            height: 28
                                            draw_text.text_style.font_size: 12
                                        }
                                    }

                                    // 行池：设备 0..2
                                    // 每行一组「连接 / 断开」：当前连着的那台显示「断开」，
                                    // 其余显示「连接」——两个按钮各带自己的配色，用可见性切换。
                                    dev_row_0 := SfRow {
                                        dev_name_0 := SfValue {
                                            text: ""
                                            width: Fill
                                            flow: Right
                                            max_lines: 1
                                            draw_text.text_style.font_size: 12.5
                                        }
                                        dev_disconn_0 := BtnGhost {
                                            text: "断开"
                                            width: 64
                                            height: 30
                                            draw_text.text_style.font_size: 12
                                        }
                                        dev_connect_0 := BtnAccent {
                                            text: "连接"
                                            width: 64
                                            height: 30
                                            draw_text.text_style.font_size: 12
                                        }
                                    }
                                    dev_row_1 := SfRow {
                                        dev_name_1 := SfValue {
                                            text: ""
                                            width: Fill
                                            flow: Right
                                            max_lines: 1
                                            draw_text.text_style.font_size: 12.5
                                        }
                                        dev_disconn_1 := BtnGhost {
                                            text: "断开"
                                            width: 64
                                            height: 30
                                            draw_text.text_style.font_size: 12
                                        }
                                        dev_connect_1 := BtnAccent {
                                            text: "连接"
                                            width: 64
                                            height: 30
                                            draw_text.text_style.font_size: 12
                                        }
                                    }
                                    dev_row_2 := SfRow {
                                        dev_name_2 := SfValue {
                                            text: ""
                                            width: Fill
                                            flow: Right
                                            max_lines: 1
                                            draw_text.text_style.font_size: 12.5
                                        }
                                        dev_disconn_2 := BtnGhost {
                                            text: "断开"
                                            width: 64
                                            height: 30
                                            draw_text.text_style.font_size: 12
                                        }
                                        dev_connect_2 := BtnAccent {
                                            text: "连接"
                                            width: 64
                                            height: 30
                                            draw_text.text_style.font_size: 12
                                        }
                                    }
                                    dev_empty_box := View {
                                        width: Fill
                                        height: Fit
                                        dev_empty := SfCaption {
                                            text: "正在扫描局域网设备…"
                                        }
                                    }
                                }

                                // 文件卡
                                file_card := SfCard {
                                    height: Fill
                                    file_head := View {
                                        width: Fill
                                        height: Fit
                                        flow: Right
                                        spacing: 8
                                        align: Align{y: 0.5}
                                        SfSection { text: "机内文件" width: Fill }
                                        file_count := SfCaption { text: "0 个" }
                                        file_refresh_btn := BtnGhost {
                                            text: "刷新"
                                            width: 56
                                            height: 28
                                            draw_text.text_style.font_size: 12
                                        }
                                    }

                                    file_scroll := ScrollYView {
                                        width: Fill
                                        height: Fill
                                        file_empty_box := View {
                                            width: Fill
                                            height: Fit
                                            file_empty := SfCaption {
                                                text: "连接后点「刷新」读取文件"
                                            }
                                        }
                                        file_list := View {
                                            width: Fill
                                            height: Fit
                                            flow: Down
                                            spacing: 6

                                            file_row_0 := SfRow {
                                                file_name_0 := SfValue {
                                                    text: ""
                                                    width: Fill
                                                    flow: Right
                                                    max_lines: 1
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_print_0 := BtnAccent {
                                                    text: "打印"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_del_0 := BtnDanger {
                                                    text: "删除"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                            }
                                            file_row_1 := SfRow {
                                                file_name_1 := SfValue {
                                                    text: ""
                                                    width: Fill
                                                    flow: Right
                                                    max_lines: 1
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_print_1 := BtnAccent {
                                                    text: "打印"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_del_1 := BtnDanger {
                                                    text: "删除"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                            }
                                            file_row_2 := SfRow {
                                                file_name_2 := SfValue {
                                                    text: ""
                                                    width: Fill
                                                    flow: Right
                                                    max_lines: 1
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_print_2 := BtnAccent {
                                                    text: "打印"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_del_2 := BtnDanger {
                                                    text: "删除"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                            }
                                            file_row_3 := SfRow {
                                                file_name_3 := SfValue {
                                                    text: ""
                                                    width: Fill
                                                    flow: Right
                                                    max_lines: 1
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_print_3 := BtnAccent {
                                                    text: "打印"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_del_3 := BtnDanger {
                                                    text: "删除"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                            }
                                            file_row_4 := SfRow {
                                                file_name_4 := SfValue {
                                                    text: ""
                                                    width: Fill
                                                    flow: Right
                                                    max_lines: 1
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_print_4 := BtnAccent {
                                                    text: "打印"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_del_4 := BtnDanger {
                                                    text: "删除"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                            }
                                            file_row_5 := SfRow {
                                                file_name_5 := SfValue {
                                                    text: ""
                                                    width: Fill
                                                    flow: Right
                                                    max_lines: 1
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_print_5 := BtnAccent {
                                                    text: "打印"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_del_5 := BtnDanger {
                                                    text: "删除"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                            }
                                            file_row_6 := SfRow {
                                                file_name_6 := SfValue {
                                                    text: ""
                                                    width: Fill
                                                    flow: Right
                                                    max_lines: 1
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_print_6 := BtnAccent {
                                                    text: "打印"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_del_6 := BtnDanger {
                                                    text: "删除"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                            }
                                            file_row_7 := SfRow {
                                                file_name_7 := SfValue {
                                                    text: ""
                                                    width: Fill
                                                    flow: Right
                                                    max_lines: 1
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_print_7 := BtnAccent {
                                                    text: "打印"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                                file_del_7 := BtnDanger {
                                                    text: "删除"
                                                    width: 56
                                                    height: 28
                                                    draw_text.text_style.font_size: 12
                                                }
                                            }
                                        }
                                    }

                                    // 拖放上传区：拖入文件 → 预览；点击 → 系统文件选择框。
                                    upload_zone := mod.widgets.SfDrop {}
                                }
                            }

                            // ───────── 右列 ─────────
                            right_col := View {
                                width: Fill
                                height: Fill
                                flow: Down
                                spacing: 12

                                // 状态 + 进度
                                status_card := SfCard {
                                    stat_head := View {
                                        width: Fill
                                        height: Fit
                                        flow: Right
                                        spacing: 10
                                        align: Align{y: 0.5}
                                        state_big := Label {
                                            text: "未连接"
                                            draw_text.text_style.font_size: 20
                                            draw_text.color: #xe8eef7
                                        }
                                        job_name := SfCaption {
                                            text: ""
                                            width: Fill
                                            draw_text.text_style.font_size: 12.5
                                        }
                                        progress_label := Label {
                                            text: "—"
                                            draw_text.text_style.font_size: 18
                                            draw_text.color: #x4da3ff
                                        }
                                    }

                                    print_progress := ProgressBar {
                                        width: Fill
                                        height: 8
                                    }

                                    stats_row := View {
                                        width: Fill
                                        height: Fit
                                        flow: Right
                                        spacing: 10
                                        align: Align{y: 0.5}

                                        stat_hotend := View {
                                            width: Fill
                                            height: Fit
                                            flow: Down
                                            spacing: 2
                                            SfCaption { text: "喷头" }
                                            hotend_val := SfValue { text: "—" }
                                        }
                                        stat_bed := View {
                                            width: Fill
                                            height: Fit
                                            flow: Down
                                            spacing: 2
                                            SfCaption { text: "热床" }
                                            bed_val := SfValue { text: "—" }
                                        }
                                        stat_pos := View {
                                            width: Fill
                                            height: Fit
                                            flow: Down
                                            spacing: 2
                                            SfCaption { text: "位置 X / Y / Z" }
                                            pos_val := SfValue { text: "—" }
                                        }
                                        stat_fan := View {
                                            width: Fill
                                            height: Fit
                                            flow: Down
                                            spacing: 2
                                            SfCaption { text: "风扇" }
                                            fan_val := SfValue { text: "—" }
                                        }
                                    }
                                }

                                // 控制区两卡
                                controls_row := View {
                                    width: Fill
                                    height: Fill
                                    flow: Right
                                    spacing: 12

                                    // 运动控制
                                    move_card := SfCard {
                                        height: Fill
                                        SfSection { text: "运动控制" }

                                        step_row := View {
                                            width: Fill
                                            height: Fit
                                            flow: Right
                                            spacing: 8
                                            align: Align{y: 0.5}
                                            SfCaption { text: "步长" width: 34 }
                                            step_minus := BtnGhost {
                                                text: "-"
                                                width: 36
                                                height: 32
                                                draw_text.text_style.font_size: 17
                                            }
                                            step_value := Label {
                                                text: "1 mm"
                                                width: 62
                                                draw_text.text_style.font_size: 13.5
                                                draw_text.color: #x4da3ff
                                            }
                                            step_plus := BtnGhost {
                                                text: "+"
                                                width: 36
                                                height: 32
                                                draw_text.text_style.font_size: 17
                                            }
                                        }

                                        x_row := View {
                                            width: Fill
                                            height: Fit
                                            flow: Right
                                            spacing: 8
                                            align: Align{y: 0.5}
                                            SfCaption { text: "X" width: 16 }
                                            x_minus := BtnGhost { text: "-" width: 56 height: 36 draw_text.text_style.font_size: 17 }
                                            x_plus := BtnGhost { text: "+" width: 56 height: 36 draw_text.text_style.font_size: 17 }
                                            x_pos := Label {
                                                text: "—"
                                                width: Fill
                                                draw_text.text_style.font_size: 13
                                                draw_text.color: #xcfdae8
                                            }
                                        }
                                        y_row := View {
                                            width: Fill
                                            height: Fit
                                            flow: Right
                                            spacing: 8
                                            align: Align{y: 0.5}
                                            SfCaption { text: "Y" width: 16 }
                                            y_minus := BtnGhost { text: "-" width: 56 height: 36 draw_text.text_style.font_size: 17 }
                                            y_plus := BtnGhost { text: "+" width: 56 height: 36 draw_text.text_style.font_size: 17 }
                                            y_pos := Label {
                                                text: "—"
                                                width: Fill
                                                draw_text.text_style.font_size: 13
                                                draw_text.color: #xcfdae8
                                            }
                                        }
                                        z_row := View {
                                            width: Fill
                                            height: Fit
                                            flow: Right
                                            spacing: 8
                                            align: Align{y: 0.5}
                                            SfCaption { text: "Z" width: 16 }
                                            z_minus := BtnGhost { text: "-" width: 56 height: 36 draw_text.text_style.font_size: 17 }
                                            z_plus := BtnGhost { text: "+" width: 56 height: 36 draw_text.text_style.font_size: 17 }
                                            z_pos := Label {
                                                text: "—"
                                                width: Fill
                                                draw_text.text_style.font_size: 13
                                                draw_text.color: #xcfdae8
                                            }
                                        }

                                        View {
                                            width: Fill
                                            height: 1
                                            show_bg: true
                                            draw_bg +: { color: #x202a38 }
                                        }

                                        home_btn := BtnGhost {
                                            text: "回原点 Home"
                                            width: Fill
                                            height: 34
                                            draw_text.text_style.font_size: 12.5
                                        }

                                        // 挤出 / 回抽（喷头动作，走确认闸门）
                                        extrude_row := View {
                                            width: Fill
                                            height: Fit
                                            flow: Right
                                            spacing: 8
                                            align: Align{y: 0.5}
                                            extrude_btn := BtnGhost {
                                                text: "挤出 5mm"
                                                width: Fill
                                                height: 32
                                                draw_text.text_style.font_size: 12
                                            }
                                            retract_btn := BtnGhost {
                                                text: "回抽 5mm"
                                                width: Fill
                                                height: 32
                                                draw_text.text_style.font_size: 12
                                            }
                                        }

                                        motor_btn := BtnGhost {
                                            text: "释放电机"
                                            width: Fill
                                            height: 32
                                            draw_text.text_style.font_size: 12
                                        }
                                    }

                                    // 温度与风扇
                                    temp_card := SfCard {
                                        height: Fill
                                        SfSection { text: "温度与风扇" }

                                        hotend_head := View {
                                            width: Fill
                                            height: Fit
                                            flow: Right
                                            spacing: 8
                                            align: Align{y: 0.5}
                                            SfCaption { text: "喷头目标" width: Fill }
                                            hotend_minus := BtnGhost {
                                                text: "-"
                                                width: 36
                                                height: 32
                                                draw_text.text_style.font_size: 17
                                            }
                                            hotend_tgt := SfValue {
                                                text: "200 ℃"
                                                width: 66
                                                draw_text.text_style.font_size: 13.5
                                            }
                                            hotend_plus := BtnGhost {
                                                text: "+"
                                                width: 36
                                                height: 32
                                                draw_text.text_style.font_size: 17
                                            }
                                        }

                                        bed_head := View {
                                            width: Fill
                                            height: Fit
                                            flow: Right
                                            spacing: 8
                                            align: Align{y: 0.5}
                                            SfCaption { text: "热床目标" width: Fill }
                                            bed_minus := BtnGhost {
                                                text: "-"
                                                width: 36
                                                height: 32
                                                draw_text.text_style.font_size: 17
                                            }
                                            bed_tgt := SfValue {
                                                text: "60 ℃"
                                                width: 66
                                                draw_text.text_style.font_size: 13.5
                                            }
                                            bed_plus := BtnGhost {
                                                text: "+"
                                                width: 36
                                                height: 32
                                                draw_text.text_style.font_size: 17
                                            }
                                        }

                                        fan_head := View {
                                            width: Fill
                                            height: Fit
                                            flow: Right
                                            spacing: 8
                                            align: Align{y: 0.5}
                                            SfCaption { text: "风扇速度" width: Fill }
                                            fan_minus := BtnGhost {
                                                text: "-"
                                                width: 36
                                                height: 32
                                                draw_text.text_style.font_size: 17
                                            }
                                            fan_tgt := SfValue {
                                                text: "0 %"
                                                width: 66
                                                draw_text.text_style.font_size: 13.5
                                            }
                                            fan_plus := BtnGhost {
                                                text: "+"
                                                width: 36
                                                height: 32
                                                draw_text.text_style.font_size: 17
                                            }
                                        }

                                        View {
                                            width: Fill
                                            height: 1
                                            show_bg: true
                                            draw_bg +: { color: #x202a38 }
                                        }

                                        preset_row := View {
                                            width: Fill
                                            height: Fit
                                            flow: Right
                                            spacing: 8
                                            align: Align{y: 0.5}
                                            pla_btn := BtnGhost {
                                                text: "PLA 200/60"
                                                width: Fill
                                                height: 30
                                                draw_text.text_style.font_size: 11.5
                                            }
                                            petg_btn := BtnGhost {
                                                text: "PETG 230/80"
                                                width: Fill
                                                height: 30
                                                draw_text.text_style.font_size: 11.5
                                            }
                                            cool_btn := BtnGhost {
                                                text: "关闭加热"
                                                width: Fill
                                                height: 30
                                                draw_text.text_style.font_size: 11.5
                                            }
                                        }

                                        View {
                                            width: Fill
                                            height: Fill
                                        }

                                        apply_btn := BtnAccent {
                                            text: "应用温度与风扇"
                                            width: Fill
                                            height: 36
                                            draw_text.text_style.font_size: 12.5
                                        }
                                    }
                                }
                            }
                        }

                        // ══════════ 全宽状态栏（界面最下方） ══════════
                        View {
                            width: Fill
                            height: 1
                            show_bg: true
                            draw_bg +: { color: #x18202c }
                        }
                        statusbar := View {
                            width: Fill
                            height: 38
                            flow: Right
                            spacing: 12
                            padding: Inset{left: 18 right: 14 top: 0 bottom: 0}
                            align: Align{y: 0.5}
                            show_bg: true
                            draw_bg +: { color: #x0c1118 }

                            sb_badge := Label {
                                text: "状态"
                                draw_text.text_style.font_size: 10.5
                                draw_text.color: #x4a5b70
                            }
                            msg_label := SfCaption {
                                text: "就绪"
                                width: Fill
                                // 状态栏固定一行：字号小一点 + 文案在 set_message 里截断，
                                // 免得长报错换行把 38px 的栏撑破（现场确实撞到过）。
                                draw_text.text_style.font_size: 11.5
                                draw_text.color: #x9db0c6
                            }
                            estop_btn := BtnDanger {
                                text: "急停"
                                width: 96
                                height: 26
                                draw_text.text_style.font_size: 12
                            }
                        }

                        // ══════════ 确认弹窗 ══════════
                        confirm_modal := Modal {
                            content +: {
                                RoundedView {
                                    width: 440
                                    height: Fit
                                    flow: Down
                                    spacing: 14
                                    padding: 20
                                    draw_bg +: {
                                        color: #x161d27
                                        border_radius: 14
                                        border_size: 1
                                        border_color: #x2b3646
                                    }
                                    Label {
                                        text: "确认操作"
                                        draw_text.text_style.font_size: 17
                                        draw_text.color: #xe8eef7
                                    }
                                    confirm_text := Label {
                                        text: ""
                                        width: Fill
                                        draw_text.text_style.font_size: 13
                                        draw_text.color: #x9db0c6
                                    }
                                    confirm_row := View {
                                        width: Fill
                                        height: Fit
                                        flow: Right
                                        spacing: 10
                                        align: Align{x: 1.0}
                                        confirm_no := BtnGhost {
                                            text: "取消"
                                            width: 92
                                            height: 34
                                            draw_text.text_style.font_size: 12.5
                                        }
                                        confirm_ok := BtnDanger {
                                            text: "确认执行"
                                            width: 112
                                            height: 34
                                            draw_text.text_style.font_size: 12.5
                                        }
                                    }
                                }
                            }
                        }

                        // ══════════ 上传预览弹窗 ══════════
                        preview_modal := Modal {
                            content +: {
                                RoundedView {
                                    width: 588
                                    height: Fit
                                    flow: Down
                                    spacing: 12
                                    padding: 20
                                    draw_bg +: {
                                        color: #x161d27
                                        border_radius: 14
                                        border_size: 1
                                        border_color: #x2b3646
                                    }
                                    preview_title := Label {
                                        text: "打印预览"
                                        draw_text.text_style.font_size: 17
                                        draw_text.color: #xe8eef7
                                    }
                                    // 缩略图 + 文字两栏：图是切片器嵌在 G-code 里的那张，
                                    // 没有缩略图的文件就整块隐藏。
                                    preview_body := View {
                                        width: Fill
                                        height: Fit
                                        flow: Right
                                        spacing: 14
                                        preview_thumb_box := View {
                                            width: 168
                                            height: 168
                                            preview_thumb := Image {
                                                width: Fill
                                                height: Fill
                                                // 缩略图填满方框（保持比例，多出的部分裁掉），
                                                // 48x48 和 300x300 都能占满，不歪不扁。
                                                fit: ImageFit.CropToFill
                                            }
                                        }
                                        preview_text_col := View {
                                            width: Fill
                                            height: Fill
                                            flow: Down
                                            spacing: 6
                                            preview_name := Label {
                                                text: ""
                                                width: Fill
                                                draw_text.text_style.font_size: 13
                                                draw_text.color: #x4da3ff
                                            }
                                            preview_info := SfCaption {
                                                text: ""
                                                width: Fill
                                                draw_text.text_style.font_size: 12.5
                                            }
                                            preview_note := SfCaption {
                                                text: ""
                                                width: Fill
                                                draw_text.text_style.font_size: 12
                                                draw_text.color: #x6f8098
                                            }
                                        }
                                    }
                                    preview_row := View {
                                        width: Fill
                                        height: Fit
                                        flow: Right
                                        spacing: 10
                                        align: Align{x: 1.0}
                                        preview_cancel := BtnGhost {
                                            text: "取消"
                                            width: 92
                                            height: 34
                                            draw_text.text_style.font_size: 12.5
                                        }
                                        preview_print := BtnAccent {
                                            text: "上传并开始打印"
                                            width: 150
                                            height: 34
                                            draw_text.text_style.font_size: 12.5
                                        }
                                    }
                                }
                            }
                        }

                        // ══════════ 上传进度弹窗（屏幕中央） ══════════
                        // Makepad 没有 ProgressBar，也没有运行时的 set_width：
                        // 进度条就用「底轨 + 20 段等宽实心块」，按百分比 set_visible。
                        upload_modal := Modal {
                            content +: {
                                RoundedView {
                                    width: 444
                                    height: Fit
                                    flow: Down
                                    spacing: 14
                                    padding: 22
                                    draw_bg +: {
                                        color: #x161d27
                                        border_radius: 14
                                        border_size: 1
                                        border_color: #x2b3646
                                    }
                                    up_title := Label {
                                        text: "正在上传"
                                        draw_text.text_style.font_size: 17
                                        draw_text.color: #xe8eef7
                                    }
                                    up_name := Label {
                                        text: ""
                                        width: Fill
                                        draw_text.text_style.font_size: 13
                                        draw_text.color: #x4da3ff
                                    }
                                    // 用 RoundedView 而不是 View：这版 Makepad 里
                                    // 纯 View 的 show_bg 在 Modal 内容里不落笔（量到矩形却画不出来）。
                                    up_track := RoundedView {
                                        width: 400
                                        height: 10
                                        flow: Right
                                        spacing: 0
                                        margin: 0
                                        padding: 0
                                        draw_bg +: { color: #x1f2a38 border_radius: 3 }
                                        up_seg_0 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_1 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_2 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_3 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_4 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_5 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_6 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_7 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_8 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_9 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_10 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_11 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_12 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_13 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_14 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_15 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_16 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_17 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_18 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                        up_seg_19 := RoundedView { width: 20 height: 10 margin: 0 padding: 0 draw_bg +: { color: #x4da3ff border_radius: 0 border_size: 0 } }
                                    }
                                    up_row := View {
                                        width: 400
                                        height: Fit
                                        flow: Right
                                        spacing: 10
                                        up_pct := Label {
                                            text: "0%"
                                            width: 52
                                            draw_text.text_style.font_size: 14
                                            draw_text.color: #x4da3ff
                                        }
                                        up_detail := SfCaption {
                                            text: ""
                                            width: Fill
                                            draw_text.text_style.font_size: 12
                                            draw_text.color: #x6f8098
                                        }
                                    }
                                    up_note := SfCaption {
                                        text: "上传期间请不要关闭窗口…"
                                        width: Fill
                                        draw_text.text_style.font_size: 11.5
                                        draw_text.color: #x4a5b70
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ───────────────────────────── App ─────────────────────────────

#[derive(Script, ScriptHook)]
pub struct App {
    #[live]
    ui: WidgetRef,
    /// 用 `Arc` 而不是裸 `Connection`：上传循环里要一边借用连接、一边写状态栏，
    /// 裸引用会让 `&self.conn` 和 `&mut self` 打架。
    #[rust]
    conn: Option<std::sync::Arc<Connection>>,
    #[rust]
    timer: Timer,
    #[rust]
    cli: Cli,
    #[rust]
    devices: Vec<Device>,
    #[rust]
    files: Vec<String>,
    #[rust]
    status: DeviceStatus,
    #[rust]
    connected: bool,
    #[rust]
    host: String,
    #[rust]
    serial: String,
    #[rust]
    message: String,
    #[rust]
    confirm_enabled: bool,
    #[rust]
    pending: Option<Intent>,
    #[rust]
    step_idx: usize,
    #[rust]
    tgt_hotend: f64,
    #[rust]
    tgt_bed: f64,
    #[rust]
    tgt_fan: f64,
    #[rust]
    pending_upload: Option<String>,
    /// 刚上传的文件名：在机内文件列表里置顶并标记。
    #[rust]
    pinned: Option<String>,
    /// 最近一次后处理得到的元信息（预览弹窗展示）。
    #[rust]
    last_info: Option<HeaderInfo>,
    /// 打印剩余时间。协议压根不上报时间，只能「切片器总时长 + 实测速度」自己算。
    #[rust]
    eta: Eta,
    /// 估算器的秒表：`observe` 要的是单调递增的秒数。
    #[rust]
    clock: Option<std::time::Instant>,
    /// 我们上传过的文件 → 切片器给的总时长（秒），落盘在 `file_times.txt`。
    ///
    /// 只有文件的**头部**里有时间，机内老文件读不回来（协议没有下载方向），
    /// 所以「开始打印」时靠这张表对号：对上就按总时长算，对不上就只信实测速度。
    #[rust]
    file_times: Vec<(String, f64)>,
    /// 本次打印的总时长是不是我们对号对上的（`note_print_start` 命中）。
    /// 没命中的话总时长不可信，开打时要丢掉。
    #[rust]
    eta_owned: bool,
    /// 上一帧的打印状态：用来抓「Free → 开打」这个跳变。
    #[rust]
    prev_ps: Option<PrintStatus>,
    /// 上一次画到界面上的剩余时间文案：只在真的变了才 `set_text`，
    /// 免得每帧状态都触发一次重排。
    #[rust]
    eta_text_drawn: String,
    /// 预览弹窗的主按钮语义：上传+打印，还是只开始打印。
    #[rust]
    preview_mode: PreviewMode,
    /// 进行中的分片上传。`None` = 没有上传。
    #[rust]
    upload_job: Option<UploadJob>,
    /// 上传用的快节奏 timer（12ms）：每 tick 只做「非阻塞收包 + 最多发一片」，
    /// 立刻返回，让 Makepad 有机会把进度条画出来。
    #[rust]
    upload_timer: Timer,
    /// 上传完成后需要补一次文件列表刷新（放到定时器里做，见 `handle_timer`）。
    ///
    /// 直接在上传路径里刷会把「是否开始打印」的弹窗压后几百毫秒（拉列表要握手分页），
    /// 设备半死时更久。用一个标记交给下一次 tick 即可。
    #[rust]
    refresh_after_upload: bool,
    /// 内嵌字体还没填进资源表时的重试计数（见 `fonts::install`）。
    #[rust]
    font_tries: u8,
    /// 命令行交过来、等着上传的任务（见 `ipc`）。
    ///
    /// 走后门进来的活不能打断正在跑的上传，所以先排队，等 `upload_job` 空了再取。
    #[rust]
    remote_queue: Vec<ipc::Request>,
    /// 上一次把窗口抬到前台的时间。
    ///
    /// 一次导出可能连着交来好几个文件（多 plate），每来一个就抢一次焦点会变成
    /// 「窗口疯狂闪」，所以设个冷却：同一批只抬一次。
    #[rust]
    last_raise: Option<std::time::Instant>,
    /// 命令行落地的交接副本目录，上传收尾时删掉（见 `handoff::stage_copy`）。
    #[rust]
    staged_dir: Option<std::path::PathBuf>,
    /// 上一次给上传名加的时间戳，以及同一秒内的序号。
    ///
    /// 戳精确到秒就够用，但多 plate 一次导出可能在同一秒里连着上传好几个文件，
    /// 光靠秒会撞回同名（设备同名不覆盖、照旧打旧的）。撞了就补 `-2` / `-3`。
    #[rust]
    last_stamp: String,
    #[rust]
    stamp_seq: u32,
    /// 后台连接线程的收件箱。`Some` = 连接进行中，此时不消费交接队列、不重复发起。
    #[rust]
    connect_rx: Option<std::sync::mpsc::Receiver<ConnectOutcome>>,
    /// 后台 UDP 扫描的收件箱。
    #[rust]
    discover_rx: Option<std::sync::mpsc::Receiver<std::io::Result<Vec<Device>>>>,
    /// 启动后的第一次扫描结束时自动连第一台设备（手动点「重新扫描」不会）。
    #[rust]
    auto_connect_after_discover: bool,
    /// 上一次自动重试连接的时间。
    #[rust]
    last_connect_try: Option<std::time::Instant>,
    /// 连续重试次数，写进状态栏让用户知道界面没死。
    #[rust]
    connect_attempts: u32,
    /// 用户主动点了「断开」：暂停自动重连，直到下一次新交接到达。
    /// 否则刚断开 5 秒又被连回去，等于没尊重用户的操作。
    #[rust]
    auto_retry_paused: bool,
}

/// 上传成功之后做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadThen {
    /// 拖放/预览里已经确认过了：直接开打。
    PrintNow,
    /// 命令行带来的文件：先问一句。
    AskPrint,
}

/// 上传分两个阶段：先等设备对 `9/1` 的应答，再一片片推数据。
///
/// 以前 `9/1` 的应答是**同步**等的（最长 10 秒），设备不理人时界面直接白屏——
/// 进度弹窗都还没画出来。现在两个阶段都放在 timer 里，弹窗第一帧就出现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadPhase {
    /// 已发 `9/1`，等设备确认（确认后才有必要灌数据）。
    Begin,
    /// 确认过了，开始灌分片。
    Chunks,
}

/// 在途分片：发出去了，等设备回 `9/2`。
struct Inflight {
    len: usize,
    at: std::time::Instant,
}

/// 等单个分片 ACK 的上限。超过它基本可以断定设备死了（不是慢）。
const ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
/// 等设备确认上传请求（`9/1`）的上限。
const BEGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 一次分片上传的进行中状态。
struct UploadJob {
    name: String,
    bytes: std::sync::Arc<Vec<u8>>,
    total: usize,
    /// 已发送且收到 ACK 的字节数。
    sent: usize,
    frag: usize,
    then: UploadThen,
    phase: UploadPhase,
    /// 已经发出、还在等 ACK 的那一片。
    ///
    /// 关键：**同一时刻只允许一片在途**（和官方 App 的抓包一致）。
    /// 这样上传就变成「发一片 → 下一 tick 看 ACK → 再发一片」的非阻塞状态机，
    /// 主线程每个 tick 只花几微秒，界面不会被上传拖住。
    inflight: Option<Inflight>,
    started: std::time::Instant,
    /// 上一次写进度条时的百分比（UI 只在变化时动）。
    last_pct: usize,
    /// 上一次刷进度条明细（速度/剩余时间）的时刻。
    painted_at: std::time::Instant,
    /// stderr 进度日志的下一个百分点。
    next_log: usize,
}

/// 预览弹窗的主按钮该做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PreviewMode {
    /// 拖放 / 选择文件后：上传 + 开始打印。
    #[default]
    UploadAndPrint,
    /// 命令行已经上传完了：只开始打印。
    StartOnly,
}

// ── 行池 id 解析（避免依赖 id! 的 const 性） ──

fn dev_name_id(i: usize) -> LiveId {
    match i {
        0 => id!(dev_name_0),
        1 => id!(dev_name_1),
        _ => id!(dev_name_2),
    }
}

fn dev_row_id(i: usize) -> LiveId {
    match i {
        0 => id!(dev_row_0),
        1 => id!(dev_row_1),
        _ => id!(dev_row_2),
    }
}

fn dev_connect_id(i: usize) -> LiveId {
    match i {
        0 => id!(dev_connect_0),
        1 => id!(dev_connect_1),
        _ => id!(dev_connect_2),
    }
}

fn dev_disconn_id(i: usize) -> LiveId {
    match i {
        0 => id!(dev_disconn_0),
        1 => id!(dev_disconn_1),
        _ => id!(dev_disconn_2),
    }
}

fn file_row_id(i: usize) -> LiveId {
    match i {
        0 => id!(file_row_0),
        1 => id!(file_row_1),
        2 => id!(file_row_2),
        3 => id!(file_row_3),
        4 => id!(file_row_4),
        5 => id!(file_row_5),
        6 => id!(file_row_6),
        _ => id!(file_row_7),
    }
}

fn file_name_id(i: usize) -> LiveId {
    match i {
        0 => id!(file_name_0),
        1 => id!(file_name_1),
        2 => id!(file_name_2),
        3 => id!(file_name_3),
        4 => id!(file_name_4),
        5 => id!(file_name_5),
        6 => id!(file_name_6),
        _ => id!(file_name_7),
    }
}

fn file_print_id(i: usize) -> LiveId {
    match i {
        0 => id!(file_print_0),
        1 => id!(file_print_1),
        2 => id!(file_print_2),
        3 => id!(file_print_3),
        4 => id!(file_print_4),
        5 => id!(file_print_5),
        6 => id!(file_print_6),
        _ => id!(file_print_7),
    }
}

fn file_del_id(i: usize) -> LiveId {
    match i {
        0 => id!(file_del_0),
        1 => id!(file_del_1),
        2 => id!(file_del_2),
        3 => id!(file_del_3),
        4 => id!(file_del_4),
        5 => id!(file_del_5),
        6 => id!(file_del_6),
        _ => id!(file_del_7),
    }
}

/// 进度条第 i 段的 id。
fn up_seg_id(i: usize) -> LiveId {
    match i {
        0 => id!(up_seg_0),
        1 => id!(up_seg_1),
        2 => id!(up_seg_2),
        3 => id!(up_seg_3),
        4 => id!(up_seg_4),
        5 => id!(up_seg_5),
        6 => id!(up_seg_6),
        7 => id!(up_seg_7),
        8 => id!(up_seg_8),
        9 => id!(up_seg_9),
        10 => id!(up_seg_10),
        11 => id!(up_seg_11),
        12 => id!(up_seg_12),
        13 => id!(up_seg_13),
        14 => id!(up_seg_14),
        15 => id!(up_seg_15),
        16 => id!(up_seg_16),
        17 => id!(up_seg_17),
        18 => id!(up_seg_18),
        _ => id!(up_seg_19),
    }
}

impl App {
    // ───────────── 连接 ─────────────

    fn connect_to(&mut self, cx: &mut Cx, idx: usize) {
        let dev = match self.devices.get(idx) {
            Some(d) => d.clone(),
            None => {
                self.set_message(cx, "未找到该设备，请重新扫描");
                return;
            }
        };
        // 重连前清掉旧连接，避免状态串台（清场在 connect_host 里统一做）。
        self.connect_host(cx, &dev.ip, &dev.serial);
    }

    /// 发起一次连接，**立刻返回**。
    ///
    /// 以前 `Connection::connect` 直接在这里跑：目标不可达时内核的 SYN 重试能挂
    /// 20 多秒，窗口完全不重绘，看起来就是「应用卡死」。现在 TCP 握手、读写线程
    /// 搭建、以及初次拉文件列表（分页最长约 4.7 秒）全部在后台线程完成，
    /// 主线程只在 `poll_connect` 里 `try_recv` 收结果，一次阻塞 IO 都不做。
    fn connect_host(&mut self, cx: &mut Cx, ip: &str, serial: &str) {
        if self.connect_rx.is_some() {
            self.set_message(cx, "已有一次连接正在进行，请稍候");
            return;
        }
        // 清掉旧连接：切到另一台设备时也走这里，避免旧会话的读数串过来。
        if let Some(c) = &self.conn {
            let _ = c.disconnect();
        }
        self.conn = None;
        self.connected = false;

        let (tx, rx) = std::sync::mpsc::channel();
        let ip2 = ip.to_string();
        let serial2 = serial.to_string();
        let dump = std::env::var_os("SF_DUMP_LIST").is_some();
        let spawned = std::thread::Builder::new()
            .name("starfish-connect".into())
            .spawn(move || {
                let result = Connection::connect_timeout(
                    &ip2,
                    7777,
                    crate::protocol::connection::CONNECT_TIMEOUT,
                )
                .map(|conn| {
                    let mut st = None;
                    let names = fetch_file_list(&conn, &mut st, dump);
                    (conn, names, st)
                });
                let _ = tx.send(ConnectOutcome {
                    ip: ip2,
                    serial: serial2,
                    result,
                });
            });
        if spawned.is_err() {
            self.set_message(cx, "无法启动连接线程，请重试");
            self.sync(cx);
            return;
        }
        self.connect_rx = Some(rx);

        let retry = if self.connect_attempts > 0 {
            format!("（第 {} 次重试）", self.connect_attempts)
        } else {
            String::new()
        };
        let pending = if self.remote_queue.is_empty() {
            String::new()
        } else {
            format!("；{} 个待上传文件已保留", self.remote_queue.len())
        };
        self.set_message(
            cx,
            &format!("正在连接 {ip}{retry}…（后台进行，界面可继续操作）{pending}"),
        );
        self.sync(cx);
    }

    /// 收后台连接的结果。成功装上连接；失败只改状态栏，绝不在 UI 线程重试。
    fn poll_connect(&mut self, cx: &mut Cx) {
        let res = match self.connect_rx.as_ref() {
            Some(rx) => rx.try_recv(),
            None => return,
        };
        let outcome = match res {
            Ok(o) => o,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.connect_rx = None;
                self.set_message(cx, "连接线程意外退出，请重试连接");
                self.sync(cx);
                return;
            }
        };
        self.connect_rx = None;

        match outcome.result {
            Ok((conn, names, st)) => {
                self.host = outcome.ip.clone();
                self.serial = outcome.serial.clone();
                self.connected = true;
                self.conn = Some(std::sync::Arc::new(conn));
                self.files.clear();
                for n in names {
                    if !self.files.contains(&n) {
                        self.files.push(n);
                    }
                }
                if let Some(st) = st {
                    self.note_status(&st);
                    self.status = st;
                }
                // 记住这台设备：UDP 发现偶尔不响应，下次启动仍能一键连接。
                self.remember_device(&outcome.ip, &outcome.serial);
                if !self.devices.iter().any(|d| d.ip == outcome.ip) {
                    self.devices.insert(
                        0,
                        Device {
                            ip: outcome.ip.clone(),
                            serial: outcome.serial.clone(),
                        },
                    );
                }
                self.connect_attempts = 0;
                self.last_connect_try = None;
                let resume = if self.remote_queue.is_empty() {
                    String::new()
                } else {
                    format!("，继续上传 {} 个待处理文件", self.remote_queue.len())
                };
                self.set_message(
                    cx,
                    &format!(
                        "已连接 {}（{} 个机内文件）{resume}",
                        outcome.ip,
                        self.files.len()
                    ),
                );
                // 不用在这里手动触发上传：下一个 12ms tick 的 drain_remote
                // 看到 connected=true 就会自动取队列里的请求。
                self.sync(cx);
            }
            Err(e) => {
                self.connected = false;
                self.conn = None;
                let pending = if self.remote_queue.is_empty() {
                    String::new()
                } else {
                    format!(
                        "；{} 个待上传文件已保留，设备上线后自动继续",
                        self.remote_queue.len()
                    )
                };
                self.set_message(cx, &format!("连接 {} 失败：{e}{pending}", outcome.ip));
                self.sync(cx);
            }
        }
    }

    // ───────────── 记住已知设备（UDP 发现不可靠时的兜底） ─────────────

    fn known_hosts_path() -> std::path::PathBuf {
        let base = std::env::var("APPDATA")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_else(|_| ".".to_string());
        let dir = std::path::Path::new(&base).join("starfish-control");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("known_devices.txt")
    }

    fn load_known_devices() -> Vec<Device> {
        let mut out: Vec<Device> = Vec::new();
        if let Ok(text) = std::fs::read_to_string(Self::known_hosts_path()) {
            for line in text.lines() {
                let mut it = line.split('|');
                let ip = it.next().unwrap_or("").trim().to_string();
                let serial = it.next().unwrap_or("").trim().to_string();
                if !ip.is_empty() && !out.iter().any(|d| d.ip == ip) {
                    out.push(Device { ip, serial });
                }
            }
        }
        out
    }

    /// 成功连接后把 (ip, serial) 记入已知列表并落盘。
    fn remember_device(&mut self, ip: &str, serial: &str) {
        if ip.is_empty() {
            return;
        }
        let mut all = Self::load_known_devices();
        if !all.iter().any(|d| d.ip == ip) {
            all.push(Device {
                ip: ip.to_string(),
                serial: serial.to_string(),
            });
        }
        let text = all
            .iter()
            .map(|d| format!("{}|{}", d.ip, d.serial))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(Self::known_hosts_path(), text);
    }

    // ───────────── 轮询 ─────────────

    /// 非阻塞地抽干已到达的帧并刷新 UI。`budget_ms` 是本次最多等待的时间。
    fn poll(&mut self, cx: &mut Cx, budget_ms: u64) {
        // 取一份 Arc 而不是借 `conn`：断线时要回头改 self。
        let conn = match self.conn.clone() {
            Some(c) => c,
            None => return,
        };
        // 链路自己断了（设备关机 / 拔网线 / 被掐）：先把 UI 退回「未连接」，
        // 否则顶上还挂着「已连接 · 空闲」、设备行还写着「断开」，全是假状态。
        if conn.is_closed() || conn.status_query().is_err() {
            self.drop_dead_link(cx);
            return;
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(budget_ms);
        let mut got_status = false;

        while std::time::Instant::now() < deadline {
            let p = match conn.recv(std::time::Duration::from_millis(2)) {
                Some(p) => p,
                None => break,
            };
            if p.len() >= 39 && p[0] == 8 && p[1] == 0 {
                if let Some(st) = DeviceStatus::from_payload(&p) {
                    self.note_status(&st);
                    self.status = st;
                    got_status = true;
                }
            }
            // cmd 10（文件列表）只由 refresh_files 的分页流程处理，这里忽略，
            // 否则会把分页元数据当文件名。
        }

        if got_status {
            self.sync(cx);
        }
    }

    /// 链路自己断了（设备关机 / 拔网线 / 被设备掐掉）：把界面退回「未连接」。
    ///
    /// 不这么做的话，顶上胶囊还写着「已连接 · 空闲」、设备行的按钮还是「断开」，
    /// 点下去只会得到一个已经死掉的 socket —— 用户看到的就是假状态。
    fn drop_dead_link(&mut self, cx: &mut Cx) {
        if let Some(c) = &self.conn {
            let _ = c.disconnect();
        }
        self.conn = None;
        self.connected = false;
        self.files.clear();
        self.status = DeviceStatus::default();
        self.eta.reset_progress();
        // 断了就不知道设备在打什么，总时长也不能沿用。
        self.eta.clear_total();
        self.eta_owned = false;
        self.prev_ps = None;
        self.set_message(cx, "设备断开了连接，点设备行的「连接」可重连");
        self.sync(cx);
    }

    /// 主动刷新文件列表。
    ///
    /// 协议是**分页拉取**的（这点很反直觉，之前漏了导致文件列表永远为空）：
    ///   `10/1` 只是应答 → 必须回 `10/2` 请求第一页
    ///   `10/2` 里 `[4..8]` 是名字块长度 n，名字在 `[8..8+n]`，随后再发 `10/2` 要下一页
    ///   收完发 `10/3` 结束列表
    fn refresh_files(&mut self, cx: &mut Cx) {
        self.refresh_files_inner(cx, false)
    }

    /// 静默刷新：只更新列表，不改状态栏那句话。
    ///
    /// 上传成功后要刷列表，但那时状态栏刚写上「已上传 X，是否开始打印？」，
    /// 不能让「文件列表已更新（N 个）」把它顶掉。
    fn refresh_files_quiet(&mut self, cx: &mut Cx) {
        self.refresh_files_inner(cx, true)
    }

    fn refresh_files_inner(&mut self, cx: &mut Cx, quiet: bool) {
        let conn = match &self.conn {
            Some(c) => c,
            None => {
                self.set_message(cx, "请先连接设备");
                return;
            }
        };
        self.files.clear();

        let mut st = None;
        let dump = std::env::var_os("SF_DUMP_LIST").is_some();
        let names = fetch_file_list(conn, &mut st, dump);
        if let Some(st) = st {
            self.note_status(&st);
            self.status = st;
        }
        for name in names {
            if !self.files.contains(&name) {
                self.files.push(name);
            }
        }

        // 刚上传的那个置顶：用户不用在列表里翻找自己刚传的文件。
        if let Some(pin) = &self.pinned {
            if let Some(idx) = self.files.iter().position(|n| n == pin) {
                if idx > 0 {
                    let name = self.files.remove(idx);
                    self.files.insert(0, name);
                }
            }
        }

        if !quiet {
            self.set_message(cx, &format!("文件列表已更新（{} 个）", self.files.len()));
        }
        self.sync(cx);
    }

    // ───────────── UI 同步 ─────────────

    fn set_message(&mut self, cx: &mut Cx, msg: &str) {
        self.message = msg.to_string();
        let shown = fit_status(msg);
        self.ui.label(cx, &[id!(msg_label)]).set_text(cx, &shown);
    }

    fn state_kind(&self) -> StateKind {
        StateKind::of(self.connected, self.status.print_status)
    }

    /// 顶栏胶囊 / 右卡大标题共用的那一个状态词。
    fn state_word(&self) -> String {
        if !self.connected {
            return "未连接".to_string();
        }
        match self.status.print_status {
            // 连着但没活干：明确写出「已连接」，不然用户看不出连没连上。
            PrintStatus::Free => "已连接 · 空闲".to_string(),
            s => zh_state(s).to_string(),
        }
    }

    /// 把 Rust 状态整体推给静态 widget 树。
    fn sync(&mut self, cx: &mut Cx) {
        // 顶栏
        let host_line = if self.connected {
            if self.serial.is_empty() {
                self.host.clone()
            } else {
                format!("{}  ·  SN {}", self.host, self.serial)
            }
        } else {
            "未连接设备".to_string()
        };
        self.ui
            .label(cx, &[id!(host_label)])
            .set_text(cx, &host_line);

        // 顶栏状态灯：4 个胶囊切可见性，文案只写在当前那个上。
        let kind = self.state_kind();
        self.ui
            .view(cx, &[id!(pill_off)])
            .set_visible(cx, kind == StateKind::Off);
        self.ui
            .view(cx, &[id!(pill_idle)])
            .set_visible(cx, kind == StateKind::Idle);
        self.ui
            .view(cx, &[id!(pill_run)])
            .set_visible(cx, kind == StateKind::Run);
        self.ui
            .view(cx, &[id!(pill_warn)])
            .set_visible(cx, kind == StateKind::Warn);

        // 「当前状态」只有一个词源：顶栏胶囊、右卡大标题、左列设备行都读它，
        // 免得出现「顶上写着打印中、右边还写着空闲」这种对不上的情况。
        let state_zh = self.state_word();
        let (pill_txt, big) = match kind {
            StateKind::Off => (id!(pill_off_text), id!(state_big)),
            StateKind::Idle => (id!(pill_idle_text), id!(state_big)),
            StateKind::Run => (id!(pill_run_text), id!(state_big)),
            StateKind::Warn => (id!(pill_warn_text), id!(state_big)),
        };
        self.ui.label(cx, &[pill_txt]).set_text(cx, &state_zh);
        self.ui.label(cx, &[big]).set_text(cx, &state_zh);

        // 进度
        let progress = if self.connected {
            self.status.progress as f64
        } else {
            0.0
        };
        self.ui
            .progress_bar(cx, &[id!(print_progress)])
            .set_value(cx, (progress / 100.0).clamp(0.0, 1.0));
        let ptext = if self.connected {
            format!("{progress:.1}%")
        } else {
            "—".to_string()
        };
        self.ui
            .label(cx, &[id!(progress_label)])
            .set_text(cx, &ptext);

        // 剩余时间（自己算的，所以带「约」）。文案没变就不碰 widget，省一次重排。
        let eta_text = self.eta_line().unwrap_or_default();
        if eta_text != self.eta_text_drawn {
            self.eta_text_drawn = eta_text.clone();
            self.ui.label(cx, &[id!(job_name)]).set_text(cx, &eta_text);
        }

        // 实时读数
        if self.connected {
            let st = &self.status;
            self.ui
                .label(cx, &[id!(hotend_val)])
                .set_text(cx, &format!("{}℃ / {}℃", st.hotend_temp, st.hotend_target));
            self.ui
                .label(cx, &[id!(bed_val)])
                .set_text(cx, &format!("{}℃ / {}℃", st.bed_temp, st.bed_target));
            self.ui.label(cx, &[id!(pos_val)]).set_text(
                cx,
                &format!(
                    "{:.1} / {:.1} / {:.1}",
                    st.pos_x as f32 / 1000.0,
                    st.pos_y as f32 / 1000.0,
                    st.pos_z as f32 / 1000.0
                ),
            );
            self.ui
                .label(cx, &[id!(fan_val)])
                .set_text(cx, &format!("{}%", st.fan_speed & 0xff));
            self.ui
                .label(cx, &[id!(x_pos)])
                .set_text(cx, &format!("{:.2} mm", st.pos_x as f32 / 1000.0));
            self.ui
                .label(cx, &[id!(y_pos)])
                .set_text(cx, &format!("{:.2} mm", st.pos_y as f32 / 1000.0));
            self.ui
                .label(cx, &[id!(z_pos)])
                .set_text(cx, &format!("{:.2} mm", st.pos_z as f32 / 1000.0));
        } else {
            // 断线后必须把读数清掉：留着最后那几个数字，看起来像还在连着。
            for lid in [
                id!(hotend_val),
                id!(bed_val),
                id!(pos_val),
                id!(fan_val),
                id!(x_pos),
                id!(y_pos),
                id!(z_pos),
            ] {
                self.ui.label(cx, &[lid]).set_text(cx, "—");
            }
        }

        // 设备行池：连着的那台标 ● 并把按钮换成「断开」，其余还是「连接」
        for i in 0..DEV_ROWS {
            let has = i < self.devices.len();
            self.ui.view(cx, &[dev_row_id(i)]).set_visible(cx, has);
            if has {
                let is_cur = self.connected && self.devices[i].ip == self.host;
                let label = if is_cur {
                    format!("● {}  ·  已连接", self.devices[i].ip)
                } else if self.devices[i].serial.is_empty() {
                    self.devices[i].ip.clone()
                } else {
                    format!("{}  ·  {}", self.devices[i].ip, self.devices[i].serial)
                };
                self.ui.label(cx, &[dev_name_id(i)]).set_text(cx, &label);
                self.ui
                    .button(cx, &[dev_connect_id(i)])
                    .set_visible(cx, !is_cur);
                self.ui
                    .button(cx, &[dev_disconn_id(i)])
                    .set_visible(cx, is_cur);
            }
        }
        self.ui
            .view(cx, &[id!(dev_empty_box)])
            .set_visible(cx, self.devices.is_empty() && !self.connected);
        if self.devices.is_empty() && !self.connected {
            self.ui
                .label(cx, &[id!(dev_empty)])
                .set_text(cx, "正在扫描局域网设备…");
        }

        // 文件行池
        for i in 0..FILE_ROWS {
            let has = i < self.files.len();
            self.ui.view(cx, &[file_row_id(i)]).set_visible(cx, has);
            if has {
                let raw = &self.files[i];
                let marked = if self.pinned.as_deref() == Some(raw.as_str()) {
                    format!("● {}", elide_name(raw, 18))
                } else {
                    elide_name(raw, 20)
                };
                self.ui.label(cx, &[file_name_id(i)]).set_text(cx, &marked);
            }
        }
        self.ui
            .view(cx, &[id!(file_empty_box)])
            .set_visible(cx, self.files.is_empty());
        self.ui.label(cx, &[id!(file_empty)]).set_text(
            cx,
            if self.connected {
                "暂无文件"
            } else {
                "连接后点「刷新」读取文件"
            },
        );
        let count_text = if self.files.len() > FILE_ROWS {
            format!("{} 个（显示前 {}）", self.files.len(), FILE_ROWS)
        } else {
            format!("{} 个", self.files.len())
        };
        self.ui
            .label(cx, &[id!(file_count)])
            .set_text(cx, &count_text);

        // 步长
        self.ui
            .label(cx, &[id!(step_value)])
            .set_text(cx, &format!("{} mm", fmt_step(STEPS[self.step_idx])));

        // 温度目标读数
        self.ui
            .label(cx, &[id!(hotend_tgt)])
            .set_text(cx, &format!("{:.0} ℃", self.tgt_hotend));
        self.ui
            .label(cx, &[id!(bed_tgt)])
            .set_text(cx, &format!("{:.0} ℃", self.tgt_bed));
        self.ui
            .label(cx, &[id!(fan_tgt)])
            .set_text(cx, &format!("{:.0} %", self.tgt_fan));

        // 状态栏：没有消息时给一句「下一步做什么」，不留空白
        let line = if self.message.is_empty() {
            if self.connected {
                "已连接：拖入 .gcode 可上传，或点左侧「刷新」读取机内文件".to_string()
            } else {
                "未连接：点左侧设备行的「连接」".to_string()
            }
        } else {
            self.message.clone()
        };
        self.ui.label(cx, &[id!(msg_label)]).set_text(cx, &line);
    }

    // ───────────── 动作执行 ─────────────

    /// 确认闸门：危险动作且开关开启 → 弹窗；否则直接执行。
    fn dispatch(&mut self, cx: &mut Cx, intent: Intent) {
        if self.confirm_enabled && intent.is_dangerous() {
            let desc = intent.describe();
            self.pending = Some(intent);
            self.ui.label(cx, &[id!(confirm_text)]).set_text(cx, &desc);
            self.ui.modal(cx, &[id!(confirm_modal)]).open(cx);
        } else {
            self.execute(cx, intent);
        }
    }

    fn execute(&mut self, cx: &mut Cx, intent: Intent) {
        // 上传进行中：挡住一切并发的设备写操作（含 `--yes` 的自动开打）。
        // 这不只是「排队」问题——`wait_ack` 会丢弃不匹配的应答帧，
        // 插进来的请求既可能不生效，也可能让上传的 ACK 序列错位。
        if self.upload_job.is_some() && intent.writes_device() {
            self.set_message(cx, "上传进行中，请等它传完再操作");
            return;
        }
        match intent {
            Intent::Connect(i) => self.connect_to(cx, i),
            Intent::Print(name) => {
                if let Some(c) = &self.conn {
                    let _ = c.print_start(0, &name);
                    self.set_message(cx, &format!("已发送开始打印：{name}"));
                }
                self.note_print_start(&name);
            }
            Intent::Delete(name) => {
                if let Some(c) = &self.conn {
                    let _ = c.file_delete(0, &name);
                }
                self.set_message(cx, &format!("已删除：{name}"));
                self.refresh_files(cx);
            }
            Intent::Home => {
                if let Some(c) = &self.conn {
                    let _ = c.control_home();
                }
                self.set_message(cx, "已发送回原点（三轴依次回零）");
            }
            Intent::UploadPrint(path) => self.do_upload_print(cx, &path),
            Intent::UploadOnly(path) => self.do_upload_only(cx, &path),
            Intent::ApplyEnv => {
                if let Some(c) = &self.conn {
                    let _ = c.control_temp(0, self.tgt_hotend.max(0.0) as u16);
                    let _ = c.control_temp(2, self.tgt_bed.max(0.0) as u16);
                    let _ = c.control_fan(0, self.tgt_fan.clamp(0.0, 100.0) as u8);
                }
                self.set_message(
                    cx,
                    &format!(
                        "已发送：喷头 {:.0}℃ / 热床 {:.0}℃ / 风扇 {:.0}%",
                        self.tgt_hotend, self.tgt_bed, self.tgt_fan
                    ),
                );
            }
            Intent::Jog(axis, dir) => {
                let um = (STEPS[self.step_idx] * 1000.0 * dir as f64) as i32;
                if let Some(c) = &self.conn {
                    let _ = c.control_move(axis, um);
                }
                let an = ["X", "Y", "Z"].get(axis as usize).copied().unwrap_or("?");
                self.set_message(
                    cx,
                    &format!(
                        "{an} {} {} mm",
                        if dir > 0 { "+" } else { "-" },
                        fmt_step(STEPS[self.step_idx])
                    ),
                );
            }
            Intent::Extrude(um) => {
                if let Some(c) = &self.conn {
                    let _ = c.control_extrude(0, um);
                }
                self.set_message(
                    cx,
                    &format!(
                        "已发送{} {:.1} mm",
                        if um > 0 { "挤出" } else { "回抽" },
                        (um as f64 / 1000.0).abs()
                    ),
                );
            }
            Intent::MotorOff => {
                if let Some(c) = &self.conn {
                    let _ = c.control_motor_off();
                }
                self.set_message(cx, "已释放电机（步进失力，注意喷头/平台下沉）");
            }
            Intent::Disconnect => {
                if let Some(c) = &self.conn {
                    let _ = c.disconnect();
                }
                self.conn = None;
                self.connected = false;
                self.files.clear();
                self.status = DeviceStatus::default();
                // 估算器的采样是按「这次连着看到的进度」攒的，断开就作废。
                self.eta.reset_progress();
                self.eta.clear_total();
                self.eta_owned = false;
                self.prev_ps = None;
                self.auto_retry_paused = true;
                self.set_message(cx, "已断开连接（待上传文件保留，新导出或手动连接后继续）");
                self.sync(cx);
            }
        }
    }

    /// 上传并立即打印（拖放 / 选择文件后确认过的那条路）。
    fn do_upload_print(&mut self, cx: &mut Cx, path: &str) {
        let Some((name, bytes)) = self.prepare_upload(cx, path) else {
            return;
        };
        self.begin_upload(cx, name, bytes, UploadThen::PrintNow);
    }

    /// 只上传，上传完把文件置顶并问「要不要开始打印」。
    ///
    /// 命令行带文件启动就走这条：Orca 后处理 → 转换 → 连接 → 上传 → 询问。
    fn do_upload_only(&mut self, cx: &mut Cx, path: &str) {
        let Some((name, bytes)) = self.prepare_upload(cx, path) else {
            return;
        };
        self.begin_upload(cx, name, bytes, UploadThen::AskPrint);
    }

    /// 上传完成后那个「是否开始打印」的弹窗（缩略图 + 文件信息）。
    ///
    /// 复用预览弹窗：同一张图、同一份元信息，只是主按钮换成「开始打印」，
    /// 按下后只发 `print_start`，不会二次上传。
    fn open_start_ask(&mut self, cx: &mut Cx, name: &str) {
        self.preview_mode = PreviewMode::StartOnly;
        self.pending_upload = Some(name.to_string());
        let (summary, thumb, lines, thumb_desc) = match &self.last_info {
            Some(i) => (
                i.summary(),
                i.thumbnail.clone(),
                i.line_count as f64 / 10000.0,
                match &i.thumbnail {
                    Some(t) => format!("{}×{}", t.width, t.height),
                    None => "无".to_string(),
                },
            ),
            None => (String::new(), None, 0.0, "无".to_string()),
        };

        self.ui
            .label(cx, &[id!(preview_title)])
            .set_text(cx, "已上传，是否开始打印？");
        // 文件名单独一行（长名字不会把元信息挤掉），下面才是切片参数。
        self.ui.label(cx, &[id!(preview_name)]).set_text(cx, name);
        self.ui
            .label(cx, &[id!(preview_info)])
            .set_text(cx, &summary);
        self.ui
            .label(cx, &[id!(preview_note)])
            .set_text(cx, &format!("机内 · {lines:.1} 万行 · 缩略图 {thumb_desc}"));
        self.ui
            .button(cx, &[id!(preview_print)])
            .set_text(cx, "开始打印");

        self.show_thumb(cx, thumb.as_ref());
        self.ui.modal(cx, &[id!(preview_modal)]).open(cx);
    }

    /// 记下一个文件的切片器总时长，供之后「开始打印」对号用。
    ///
    /// 这是全流程里**唯一准确的剩余时间来源**：文件是我们传上去的，头部里的
    /// `;TIME:` 就是切片器排完路径算出来的真实时长。落盘保存，软件重启后
    /// 再打印同一份机内文件仍然认得出它。
    fn remember_time(&mut self, name: &str, secs: f64) {
        if !secs.is_finite() || secs <= 0.0 {
            return;
        }
        if let Some(slot) = self.file_times.iter_mut().find(|(n, _)| n == name) {
            slot.1 = secs;
        } else {
            self.file_times.push((name.to_string(), secs));
            if self.file_times.len() > 24 {
                self.file_times.remove(0);
            }
        }
        self.save_file_times();
    }

    fn file_time(&self, name: &str) -> Option<f64> {
        self.file_times
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, s)| *s)
    }

    fn file_times_path() -> std::path::PathBuf {
        let base = std::env::var("APPDATA")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_else(|_| ".".to_string());
        let dir = std::path::Path::new(&base).join("starfish-control");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("file_times.txt")
    }

    /// 一行一个 `文件名|秒`。文件名在 `9/1` 里是裸字节，我们上传前已转成 ASCII。
    fn load_file_times() -> Vec<(String, f64)> {
        let mut out: Vec<(String, f64)> = Vec::new();
        if let Ok(text) = std::fs::read_to_string(Self::file_times_path()) {
            for line in text.lines() {
                let Some((name, secs)) = line.rsplit_once('|') else {
                    continue;
                };
                let name = name.trim();
                let Ok(secs) = secs.trim().parse::<f64>() else {
                    continue;
                };
                if !name.is_empty() && secs.is_finite() && secs > 0.0 {
                    out.push((name.to_string(), secs));
                }
            }
        }
        out.truncate(24);
        out
    }

    fn save_file_times(&self) {
        let text = self
            .file_times
            .iter()
            .map(|(n, s)| format!("{n}|{s:.1}"))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(Self::file_times_path(), text);
    }

    /// 开始打印时调一次：对上文件就按它的总时长算，对不上就清掉，
    /// 免得拿上一个文件的时间硬套（那会得出一个很自信的错数字）。
    fn note_print_start(&mut self, name: &str) {
        match self.file_time(name) {
            Some(secs) => {
                self.eta.set_total(secs);
                self.eta_owned = true;
            }
            None => {
                self.eta.clear_total();
                self.eta_owned = false;
            }
        }
        self.eta.reset_progress();
    }

    /// 每来一帧 8/0 状态喂一次估算器（进度没变时它自己会忽略）。
    fn note_status(&mut self, st: &DeviceStatus) {
        let ps = st.print_status;
        let was_free = matches!(self.prev_ps, None | Some(PrintStatus::Free));
        let started = was_free && ps != PrintStatus::Free;
        if ps == PrintStatus::Free {
            // 打完了 / 没在打：采样留着没意义，下次开打重新攒。
            self.eta.reset_progress();
            self.eta_owned = false;
            self.prev_ps = Some(ps);
            return;
        }
        if started && !self.eta_owned {
            // 不是本机点「开始打印」发起的（多半是在设备屏幕上按的），
            // 打的是哪个文件我们无从得知 → 上一份文件的总时长不能拿来用。
            self.eta.clear_total();
        }
        self.prev_ps = Some(ps);
        let t0 = *self.clock.get_or_insert_with(std::time::Instant::now);
        self.eta.observe(t0.elapsed().as_secs_f64(), st.progress);
    }

    /// 右卡「打印中」旁边那行小字：`剩余约 1h12m · 共 2h48m`。
    ///
    /// 有切片器总时长（本机上传的文件）就按它算，并标出「共」多少；
    /// 机内老文件拿不到头部，只能用实测速度外推，末尾标「实测估算」说明精度不同。
    /// 两个来源都没有就老实写「估算中」，宁可不显示也不编一个数字。
    fn eta_line(&self) -> Option<String> {
        if !self.connected {
            return None;
        }
        let st = &self.status;
        let printing = matches!(
            st.print_status,
            PrintStatus::Printing
                | PrintStatus::Pause
                | PrintStatus::Pausing
                | PrintStatus::PauseResume
                | PrintStatus::EndingForPrint
                | PrintStatus::ErrorPause
                | PrintStatus::ErrorPausing
                | PrintStatus::FilamentPause
                | PrintStatus::FilamentPausing
                | PrintStatus::ReadFileAbnormalPause
                | PrintStatus::ReadFileAbnormalPausing
        );
        if !printing {
            return None;
        }
        let mut parts: Vec<String> = Vec::new();
        match self.eta.remaining(st.progress) {
            Some(secs) => parts.push(format!("剩余约 {}", fmt_duration(secs))),
            None => parts.push("剩余时间估算中".to_string()),
        }
        match self.eta.total() {
            Some(total) => parts.push(format!("共 {}", fmt_duration(total))),
            None => {
                if self.eta.remaining(st.progress).is_some() {
                    parts.push("实测估算".to_string());
                }
            }
        }
        Some(parts.join(" · "))
    }

    /// 读文件 → 后处理成 Starfish 头部 →（可选写回）→ 备好要上传的字节。
    fn prepare_upload(&mut self, cx: &mut Cx, path: &str) -> Option<(String, Vec<u8>)> {
        let raw = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.set_message(cx, &format!("读取文件失败：{e}"));
                return None;
            }
        };
        let text = String::from_utf8_lossy(&raw);
        let converted = header::convert(&text, self.cli.material.as_deref());
        let info = converted.info.clone();

        if self.cli.rewrite {
            let p = std::path::Path::new(path);
            let backup = p.with_extension(format!(
                "{}orca-original",
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| format!("{e}."))
                    .unwrap_or_default()
            ));
            if !backup.exists() {
                let _ = std::fs::copy(p, &backup);
            }
            if let Err(e) = std::fs::write(p, converted.output.as_bytes()) {
                self.set_message(cx, &format!("写回文件失败：{e}"));
                return None;
            }
        }

        let raw_name = std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("upload.gcode");
        // 文件名在 `9/1` 里是**裸字节**（没有长度字段，靠 payload 结尾界定），
        // 固件多半按单字节处理。中文/空格/控制字符写进去会变成列表里的乱码，
        // 严重时名字被截断到和我们记录的名字对不上（后续 print_start 就找不到文件）。
        let (name, _ascii_fixed) = sanitize_name(raw_name);
        // 设备端**同名不会覆盖**：新内容传上去，开始打印还是上一次的刀路（实测）。
        // 所以每次上传都缀一个本地时间戳（精确到秒），让名字天然唯一。
        // 切片器给的名字里那种 `43m31s` 是**打印时长**，不能当时间戳用。
        let now = naming::timestamp_now();
        let (full_stamp, seq) = naming::next_stamp(&now, &self.last_stamp, self.stamp_seq);
        self.last_stamp = now;
        self.stamp_seq = seq;
        let name = naming::stamp(&name, &full_stamp);

        let body = converted.output;
        if body.trim().is_empty() {
            self.set_message(cx, &format!("{name} 后处理后没有内容，已取消上传"));
            return None;
        }

        // 界面上让人一眼看出「原名 → 实际传上去的名字」，时间戳是自己加的，
        // 不写清楚会让人以为是切片器起的名字。
        let shown = if name != raw_name {
            format!("{raw_name} → 上传名 {name}")
        } else {
            name.clone()
        };
        self.set_message(cx, &format!("已后处理 {shown}：{}", info.summary()));
        // 头部里的总时长先记下来：之后「开始打印」这个文件时，剩余时间就有依据。
        self.remember_time(&name, info.time_s);
        self.last_info = Some(info);
        Some((name, body.into_bytes()))
    }

    /// 开始一次上传：做完整理、发 9/1、开进度弹窗，然后交给 timer 一片片推。
    ///
    /// 为什么不是一把梭：上传要好几秒，同步跑完的话 Makepad 只能在函数返回后重绘，
    /// 进度条会永远停在 0%。所以这里只做到「握手完」，剩下的分片在 `upload_step` 里
    /// 每 20ms 推进几十 KB 就返回，界面才有机会画。
    fn begin_upload(&mut self, cx: &mut Cx, name: String, bytes: Vec<u8>, then: UploadThen) {
        let Some(c) = self.conn.clone() else {
            self.set_message(cx, "未连接设备，无法上传");
            return;
        };
        let c: &Connection = &c;
        let total = bytes.len();

        if let Some(lim) = upload_limit() {
            if total > lim {
                self.set_message(
                    cx,
                    &format!(
                        "{name} 有 {}，超过 SF_MAX_UPLOAD={}，已跳过上传",
                        kb(total),
                        kb(lim)
                    ),
                );
                self.sync(cx);
                return;
            }
        }
        if let Some(note) = size_note(total) {
            // 只提醒，不擅自替用户拒绝：机内剩余空间是变量。
            self.set_message(cx, &format!("{name}（{}）{note}", kb(total)));
            self.sync(cx);
        }

        // ⚠️ 这里**不再**无脑 `file_delete(0, name)`。
        // 实测：反复发 10/4 之后，机内 6 个文件里有 4 个消失了（名字对不上的也掉），
        // 说明 10/4 的 payload 布局（`[media, name, 0]`，来自参考实现的猜测）
        // 很可能不被固件按名字解析。删错文件是不可恢复的，所以上传前不删。
        // 同名风险改由上传名带时间戳来消除（见 `prepare_upload`），不依赖设备报错：
        // 实测同名时设备既不报错也不覆盖，而是继续打旧的那份。
        if let Err(e) = c.upload_begin(total as u32, &name) {
            self.set_message(cx, &format!("上传失败（开始）：{e}"));
            self.sync(cx);
            return;
        }

        const FRAG: usize = 20480;
        // 分片大小可用 SF_FRAG 覆盖（现场排查用；默认与参考实现一致）。
        let frag = std::env::var("SF_FRAG")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v >= 1024 && *v <= 1 << 20)
            .unwrap_or(FRAG);

        let job = UploadJob {
            name: name.clone(),
            bytes: std::sync::Arc::new(bytes),
            total,
            sent: 0,
            frag,
            then,
            phase: UploadPhase::Begin,
            inflight: None,
            started: std::time::Instant::now(),
            last_pct: usize::MAX,
            painted_at: std::time::Instant::now(),
            next_log: 0,
        };

        // 打开进度弹窗：先清空 20 段，再按 tick 点亮。
        self.ui.label(cx, &[id!(up_title)]).set_text(cx, "正在上传");
        self.ui
            .label(cx, &[id!(up_name)])
            .set_text(cx, &elide_name(&name, 40));
        self.ui.label(cx, &[id!(up_pct)]).set_text(cx, "0%");
        self.ui
            .label(cx, &[id!(up_detail)])
            .set_text(cx, &format!("0 B / {}  ·  等待设备确认…", kb(total)));
        for i in 0..SEG_COUNT {
            self.ui.view(cx, &[up_seg_id(i)]).set_visible(cx, false);
        }
        self.ui.modal(cx, &[id!(upload_modal)]).open(cx);

        self.upload_job = Some(job);
        self.set_message(cx, &format!("开始上传 {name}（{}）…", kb(total)));
        self.sync(cx);
    }

    /// timer tick：非阻塞地推进一片，立刻返回让界面重绘。
    fn upload_step(&mut self, cx: &mut Cx) {
        let Some(mut job) = self.upload_job.take() else {
            return;
        };
        let Some(conn) = self.conn.clone() else {
            self.abort_upload_offline(cx, &job);
            return;
        };
        let conn: &Connection = &conn;

        // ⚠️ 这个函数**不许阻塞**。
        //
        // 以前是「发一片 → `wait_ack` 等 8 秒 → 再发一片」，每次 tick 要占住
        // 主线程几十毫秒；timer 间隔只有 20ms，于是主线程被占满，窗口连重绘都排不上
        // （表现就是进度条一顿一顿、甚至用远程抓图都抓不到帧）。
        //
        // 现在的节奏：每 tick 只做「非阻塞收包 → 最多发一片 → 返回」，几微秒。
        // 一片在途、收到 ACK 才发下一片，和官方 App 的抓包节奏一致，吞吐不变。

        // ① 非阻塞抽干已到达的帧（`recv_timeout(0)` 立刻返回）。
        let mut ack_begin = false;
        let mut ack_frag = 0usize;
        let mut got_status = false;
        let mut dev_err: Option<String> = None;
        while let Some(p) = conn.recv(std::time::Duration::ZERO) {
            if p.len() < 2 {
                continue;
            }
            match (p[0], p[1]) {
                (8, 0) => {
                    if let Some(st) = DeviceStatus::from_payload(&p) {
                        self.status = st;
                        got_status = true;
                    }
                }
                (9, 1) => ack_begin = true,
                (9, 2) => {
                    if p.len() >= 5 && p[4] != 0 {
                        dev_err = Some(format!("设备返回错误码 {}", p[4]));
                    } else {
                        ack_frag += 1;
                    }
                }
                _ => {}
            }
        }
        if let Some(why) = dev_err {
            let (name, sent, total) = (job.name.clone(), job.sent, job.total);
            self.ui.modal(cx, &[id!(upload_modal)]).close(cx);
            self.abort_upload(cx, conn, &name, sent, total, &why);
            return;
        }

        // ② 在途分片有没有确认？没有就把它放回去继续等。
        if let Some(fl) = job.inflight.take() {
            if ack_frag > 0 {
                job.sent += fl.len;
            } else if conn.is_closed() {
                let (name, sent, total) = (job.name.clone(), job.sent, job.total);
                self.ui.modal(cx, &[id!(upload_modal)]).close(cx);
                self.abort_upload(cx, conn, &name, sent, total, "设备主动断开了连接");
                return;
            } else if fl.at.elapsed() > ACK_TIMEOUT {
                let (name, sent, total) = (job.name.clone(), job.sent, job.total);
                self.ui.modal(cx, &[id!(upload_modal)]).close(cx);
                self.abort_upload(
                    cx,
                    conn,
                    &name,
                    sent,
                    total,
                    &format!("设备 {} 秒没有应答这一片", ACK_TIMEOUT.as_secs()),
                );
                return;
            } else {
                job.inflight = Some(fl);
            }
        }

        // ③ 阶段推进：等 `9/1` 的确认。
        if job.phase == UploadPhase::Begin {
            if ack_begin {
                job.phase = UploadPhase::Chunks;
            } else if conn.is_closed() {
                let (name, total) = (job.name.clone(), job.total);
                self.ui.modal(cx, &[id!(upload_modal)]).close(cx);
                self.abort_upload(cx, conn, &name, 0, total, "设备断开了连接");
                return;
            } else if job.started.elapsed() > BEGIN_TIMEOUT {
                let (name, total) = (job.name.clone(), job.total);
                self.ui.modal(cx, &[id!(upload_modal)]).close(cx);
                self.abort_upload(cx, conn, &name, 0, total, "设备 10 秒内没有确认上传请求");
                return;
            }
        }

        // ④ 没在途分片就发下一片。心跳也在这里补——设备的看门狗是 4.01 秒，
        //    而且 **9/2 分片不给它续命**（抓包实测），全靠 `beat()` 发 8/0。
        conn.beat();
        if job.phase == UploadPhase::Chunks && job.inflight.is_none() && job.sent < job.total {
            let end = (job.sent + job.frag).min(job.total);
            // 先把 Arc 拿出来，免得一边借用 `job.bytes` 一边写 `job.inflight`。
            let bytes = job.bytes.clone();
            let chunk = &bytes[job.sent..end];
            match conn.upload_fragment(chunk.len() as u32, chunk) {
                Ok(()) => {
                    job.inflight = Some(Inflight {
                        len: chunk.len(),
                        at: std::time::Instant::now(),
                    })
                }
                Err(e) => {
                    let (name, sent, total) = (job.name.clone(), job.sent, job.total);
                    self.ui.modal(cx, &[id!(upload_modal)]).close(cx);
                    self.abort_upload(cx, conn, &name, sent, total, &format!("发送失败：{e}"));
                    return;
                }
            }
        }

        self.paint_progress(cx, &mut job);
        if got_status {
            self.sync(cx);
        }

        // ⑤ 全部发完且都确认了才算完成。
        if job.phase == UploadPhase::Chunks && job.sent >= job.total && job.inflight.is_none() {
            self.finish_upload(cx, job);
        } else {
            self.upload_job = Some(job);
        }
    }

    /// 把百分比推到进度条上。
    fn paint_progress(&mut self, cx: &mut Cx, job: &mut UploadJob) {
        let pct = job.sent * 100 / job.total.max(1);
        let pct = if job.sent >= job.total { 100 } else { pct };
        // 百分比没变也别把「速度/剩余时间」冻住：300ms 补刷一次明细。
        if pct == job.last_pct && job.painted_at.elapsed() < std::time::Duration::from_millis(300) {
            return;
        }
        job.last_pct = pct;
        job.painted_at = std::time::Instant::now();

        // 亮起的段数：至少 1 段（有进展就看得见），100% 时全亮。
        let lit = if pct >= 100 {
            SEG_COUNT
        } else {
            (pct * SEG_COUNT / 100).max(if pct > 0 { 1 } else { 0 })
        };
        for i in 0..SEG_COUNT {
            self.ui.view(cx, &[up_seg_id(i)]).set_visible(cx, i < lit);
        }

        let elapsed = job.started.elapsed().as_secs_f64();
        let speed = if elapsed > 0.05 {
            job.sent as f64 / elapsed
        } else {
            0.0
        };
        let left = job.total.saturating_sub(job.sent);
        let eta = if speed > 1024.0 * 1024.0 * 0.05 {
            format!("，约剩 {:.0}s", left as f64 / speed)
        } else {
            String::new()
        };

        self.ui
            .label(cx, &[id!(up_pct)])
            .set_text(cx, &format!("{pct}%"));
        self.ui.label(cx, &[id!(up_detail)]).set_text(
            cx,
            &if job.phase == UploadPhase::Begin {
                format!("0 B / {}  ·  等待设备确认…", kb(job.total))
            } else {
                format!(
                    "{} / {}  ·  {}/s{eta}",
                    kb(job.sent),
                    kb(job.total),
                    kb(speed.max(0.0) as usize)
                )
            },
        );

        if pct >= job.next_log {
            eprintln!(
                "[upload] {pct}%  {}/{} 字节  {:?}",
                job.sent,
                job.total,
                job.started.elapsed()
            );
            job.next_log = pct + 10;
        }
    }

    /// 全部分片发完：收尾，然后按 `then` 决定打印还是询问。
    fn finish_upload(&mut self, cx: &mut Cx, mut job: UploadJob) {
        let Some(c) = self.conn.clone() else {
            self.abort_upload_offline(cx, &job);
            return;
        };
        job.sent = job.total;
        self.paint_progress(cx, &mut job);

        if let Err(e) = c.upload_complete() {
            self.ui.modal(cx, &[id!(upload_modal)]).close(cx);
            self.abort_upload(
                cx,
                &c,
                &job.name,
                job.sent,
                job.total,
                &format!("收尾失败：{e}"),
            );
            return;
        }
        // 官方 App 不等 9/3 的 ACK，但等它有两个好处：确认设备真的把缓冲区刷完，
        // 以及这期间 `wait_ack` 会继续打心跳（免得 4 秒看门狗在收尾时咬一口）。
        let _ = c.wait_ack(9, 3, std::time::Duration::from_secs(3));
        eprintln!(
            "[upload] 完成 {}/{} 字节，耗时 {:?}",
            job.sent,
            job.total,
            job.started.elapsed()
        );
        self.ui.modal(cx, &[id!(upload_modal)]).close(cx);

        let name = job.name.clone();
        self.pinned = Some(name.clone());

        // ⚠️ 顺序很重要：先把结果交给用户，再刷列表。
        // 拉列表要「握手 0.6s + 最多两轮 1.8s」，设备半死时还会加一次重连，
        // 挡在弹窗前面就是 4 秒白屏 —— 看着像上传卡住了。
        // 现在弹窗/状态栏立刻到位，列表在背后补上。
        match job.then {
            UploadThen::PrintNow => {
                if let Some(c) = &self.conn {
                    let _ = c.print_start(0, &name);
                }
                self.note_print_start(&name);
                self.set_message(
                    cx,
                    &format!("已上传并开始打印：{name}（{}）", kb(job.total)),
                );
                self.sync(cx);
            }
            UploadThen::AskPrint => {
                if self.cli.yes {
                    if let Some(c) = &self.conn {
                        let _ = c.print_start(0, &name);
                    }
                    self.note_print_start(&name);
                    self.set_message(cx, &format!("已开始打印：{name}"));
                    self.sync(cx);
                } else {
                    self.set_message(
                        cx,
                        &format!("已上传 {name}（{}），是否开始打印？", kb(job.total)),
                    );
                    self.open_start_ask(cx, &name);
                }
            }
        }

        // 设备写完之后经常会自己把 TCP 关掉（实测提交后就是 FIN/EOF），
        // 这时直接刷新列表会拿到空结果。**不能只看 `is_closed()`**：
        // FIN 还在路上时它仍是 false，但 10/1 已经发不出去了。
        // 所以以「列表为空」为准，重连再拉一次。
        // （这里只打标记，真正的刷新在下一个 tick 做，别把弹窗压后。）
        self.refresh_after_upload = true;
        // 字节已经全进了设备，交接副本可以删了（缩略图/元信息早在 `last_info` 里）。
        self.cleanup_staged();
    }

    /// 上传完成后的列表刷新。
    ///
    /// 设备在写完 flash 之后有较大概率直接把 TCP 关掉（19:xx 的实测和抓包里都出现过），
    /// 于是 `10/1` 发出去没有回应、列表变空。这里用「结果为空」作为判据重连一次，
    /// 而不是依赖 `is_closed()`——FIN 从设备走到本机有时差，那时它还是 false。
    fn refresh_files_after_upload(&mut self, cx: &mut Cx) {
        // 静默：状态栏那句「已上传 …，是否开始打印？」不能被「列表已更新」顶掉。
        self.refresh_files_quiet(cx);
        if !self.files.is_empty() {
            return;
        }
        let host = self.host.clone();
        let serial = self.serial.clone();
        if host.is_empty() {
            return;
        }
        eprintln!("[upload] 提交后列表为空，重连 {host} 再拉一次");
        self.conn = None;
        self.connected = false;
        self.connect_host(cx, &host, &serial);
        if self.connected {
            self.refresh_files_quiet(cx);
        }
    }

    /// 连接在等 ACK 之前就没了（被断开/被拔网线）。
    fn abort_upload_offline(&mut self, cx: &mut Cx, job: &UploadJob) {
        self.ui.modal(cx, &[id!(upload_modal)]).close(cx);
        self.conn = None;
        self.connected = false;
        self.files.clear();
        self.set_message(
            cx,
            &format!(
                "上传失败（已传 {} / {}）：连接已断开，请重新连接后再试",
                kb(job.sent),
                kb(job.total)
            ),
        );
        self.cleanup_staged();
        self.sync(cx);
    }

    /// 上传失败后的清场：中止 + 删掉半截文件 +（断线时）把 UI 标记成未连接。
    fn abort_upload(
        &mut self,
        cx: &mut Cx,
        c: &Connection,
        name: &str,
        sent: usize,
        total: usize,
        why: &str,
    ) {
        let _ = c.upload_stop(); // 9/4
        std::thread::sleep(std::time::Duration::from_millis(150));
        self.upload_job = None;
        // ⚠️ 不清场删除：10/4 的语义还没确认（见 begin_upload 的注释），
        // 删错文件不可恢复，宁可留半截文件让用户在界面上自己处理。
        let closed = c.is_closed();
        let head = if closed {
            format!(
                "{name} 上传失败：设备断开（已传 {} / {}）",
                kb(sent),
                kb(total)
            )
        } else {
            format!("{name} 上传失败（已传 {} / {}）", kb(sent), kb(total))
        };
        if closed {
            self.set_message(cx, &format!("{head} · {why}。点「连接」可重连"));
        } else {
            self.set_message(cx, &format!("{head} · {why}"));
        }
        if closed {
            self.conn = None;
            self.connected = false;
            self.files.clear();
        }
        self.cleanup_staged();
        self.sync(cx);
    }

    // ───────────── headless ─────────────

    fn run_headless(&mut self) {
        let host = match &self.cli.host {
            Some(h) => h.clone(),
            None => {
                eprintln!("headless 模式需要目标：指定 --host");
                return;
            }
        };
        match Connection::connect(&host, 7777) {
            Ok(c) => {
                // 删除：先握手拿 clientId，再拉一次列表把会话建起来。
                // 冷连接直接发 10/4 会被设备 reset 掉（实测：删除没生效）。
                //
                // ⚠️ 默认停用：10/4 的 payload 布局来自参考实现的推测，
                // 实测发几次之后机内 6 个文件掉了 4 个（名字对不上的也掉）。
                // 删错文件不可恢复，必须显式开门。
                if !self.cli.delete.is_empty() && std::env::var_os("SF_ALLOW_DELETE").is_some() {
                    let mut st = None;
                    let _ = fetch_file_list(&c, &mut st, false);
                    for name in self.cli.delete.clone() {
                        match c.file_delete(0, &name) {
                            Ok(_) => {
                                std::thread::sleep(std::time::Duration::from_millis(400));
                                println!("已请求删除 {name}");
                            }
                            Err(e) => eprintln!("删除 {name} 失败：{e}"),
                        }
                    }
                    // 用一次列表确认，别只看「请求已发出」。
                    // 设备做完文件操作常把连接 reset 掉：那就重连一次再核对。
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    let mut left: Vec<String> = Vec::new();
                    if !c.is_closed() {
                        let mut st2 = None;
                        left = fetch_file_list(&c, &mut st2, false);
                    }
                    if left.is_empty() {
                        if let Ok(c2) = Connection::connect(&host, 7777) {
                            let mut st3 = None;
                            left = fetch_file_list(&c2, &mut st3, false);
                        }
                    }
                    if left.is_empty() {
                        println!("无法确认删除结果（列表请求无应答）");
                    } else {
                        for name in &self.cli.delete {
                            if left.contains(name) {
                                println!("删除失败（仍在机内）：{name}");
                            } else {
                                println!("已删除：{name}");
                            }
                        }
                    }
                } else if !self.cli.delete.is_empty() {
                    eprintln!(
                        "注意：--delete 已默认停用：10/4 的文件名解析还没验证，实测会误删别的文件。"
                    );
                    eprintln!("   确认要试的话：SF_ALLOW_DELETE=1");
                }

                // 状态和文件列表共用同一次握手：`fetch_file_list` 会把路上的 8/0 收下来。
                let mut st: Option<DeviceStatus> = None;
                let names = if self.cli.list {
                    fetch_file_list(&c, &mut st, std::env::var_os("SF_DUMP_LIST").is_some())
                } else {
                    let _ = c.status_query();
                    if let Some(p) = c.recv(std::time::Duration::from_millis(1500)) {
                        if let Some(parsed) = DeviceStatus::from_payload(&p) {
                            st = Some(parsed);
                        }
                    }
                    Vec::new()
                };

                if self.cli.status {
                    match &st {
                        Some(s) => println!(
                            "status={} hotend={}/{} bed={}/{} pos=({:.2},{:.2},{:.2}) progress={:.1}%",
                            s.print_status.label(),
                            s.hotend_temp,
                            s.hotend_target,
                            s.bed_temp,
                            s.bed_target,
                            s.pos_x as f32 / 1000.0,
                            s.pos_y as f32 / 1000.0,
                            s.pos_z as f32 / 1000.0,
                            s.progress
                        ),
                        None => eprintln!("未收到状态回包"),
                    }
                }
                if self.cli.list {
                    for name in &names {
                        println!("{name}");
                    }
                    if names.is_empty() {
                        eprintln!("机内没有文件（或列表请求无应答）");
                    }
                }
            }
            Err(e) => eprintln!("连接失败：{e}"),
        }
    }

    /// 收命令行交来的任务，空闲时执行一个。
    ///
    /// 两件事必须分清楚：
    /// - **收**：每个 tick 都收（12ms 一次），否则交接方要等；
    /// - **做**：只在没有上传在跑时做。上传期间插进来的设备写操作会让 `wait_ack`
    ///   序列错位，宁可排队。
    fn drain_remote(&mut self, cx: &mut Cx) {
        let before = self.remote_queue.len();
        for req in ipc::take_requests() {
            self.remote_queue.push(req);
        }
        if self.remote_queue.len() > before {
            // 新的导出 = 用户又想传文件了，解除手动断开后的重连暂停。
            self.auto_retry_paused = false;
            self.raise_for_handoff();
        }
        if self.upload_job.is_some() {
            return;
        }
        // 连接进行中：队列和副本都原样留着。连接结果由 poll_connect 收，
        // 成功后的下一个 tick 会自动走到下面的「已连接」分支把文件传出去。
        if self.connect_rx.is_some() {
            return;
        }
        if !self.connected {
            if self.remote_queue.is_empty() {
                // 空闲兜底清扫：确认弹窗被取消之类的残留副本。
                self.cleanup_staged();
            } else {
                // 设备没连上：文件留在队列里，低频后台重连，绝不丢。
                self.ensure_connect_for_pending(cx);
            }
            return;
        }
        if !self.remote_queue.is_empty() {
            let req = self.remote_queue.remove(0);
            self.run_remote(cx, req);
        } else {
            self.cleanup_staged();
        }
    }

    /// 有文件在等、但设备没连上：挑一台目标设备，低频异步重试。
    ///
    /// 常见场景是「先在切片器里导出，过一会儿才给打印机通电」。所以这里不报错、
    /// 不丢文件、不刷屏，每 5 秒后台试一次；连上后 `poll_connect` 把 connected
    /// 置回 true，下一个 12ms tick 就自动接着上传。
    fn ensure_connect_for_pending(&mut self, cx: &mut Cx) {
        if self.connect_rx.is_some() || self.remote_queue.is_empty() || self.auto_retry_paused {
            return;
        }
        let now = std::time::Instant::now();
        if let Some(t) = self.last_connect_try {
            if now.duration_since(t) < CONNECT_RETRY_EVERY {
                return;
            }
        }
        // 命令行点名的设备优先；否则第一台已知设备；一台都没有就后台扫一次。
        let target = pending_connect_target(&self.remote_queue, &self.devices);
        self.last_connect_try = Some(now);
        match target {
            Some(ip) => {
                self.connect_attempts += 1;
                self.connect_host(cx, &ip, "");
            }
            None => {
                if self.discover_rx.is_none() {
                    self.start_discover(cx, false);
                }
                self.set_message(cx, "等待设备连接：正在扫描局域网… 待上传文件已保留");
            }
        }
    }

    /// 有新交接时把窗口抬到前台，带冷却。
    ///
    /// 为什么抬：切片器导出完，用户的眼睛还在切片器上；界面上马上会出现
    /// 「是否开始打印」这种必须有人回答的问题，压在后面等于没有。
    ///
    /// 为什么带冷却：一次导出可能连着交来好几个文件（多 plate 每 plate 交一次），
    /// 每来一个抢一次焦点就变成窗口乱闪。冷却期内只抬第一次。
    ///
    /// 为什么只在这里抬：上传进度、上传完成这些时刻用户已经在看着窗口了，
    /// 再抬就是打扰。
    fn raise_for_handoff(&mut self) {
        const COOLDOWN: std::time::Duration = std::time::Duration::from_secs(2);
        if let Some(t) = self.last_raise {
            if t.elapsed() < COOLDOWN {
                return;
            }
        }
        self.last_raise = Some(std::time::Instant::now());
        crate::raise::attention();
    }

    /// 执行一条来自命令行的请求：上传（必要时传完直接开打）。
    ///
    /// 调用前提：`drain_remote` 已确认设备连上、且没有上传在跑。连接本身是异步的，
    /// 没连上时请求根本不会被取出来，所以这里不再处理「连接失败」。
    ///
    /// 后处理**不在这里做**：命令行进程已经改好了（`--rewrite` 的话连文件都写回了）。
    /// 这里再 `prepare_upload` 一次只是为了拿到字节和元信息，`convert` 是幂等的，
    /// 不会把头部改坏（`header::tests::idempotent_on_own_output` 盯着这一点）。
    fn run_remote(&mut self, cx: &mut Cx, req: ipc::Request) {
        if req.path.is_empty() {
            return;
        }

        // 先确认字节真能读到，再去连设备：读文件是纯本地的事，读不到连上也白连，
        // 而且此刻报错最具体（路径 + 系统错误码），不会被后面的连接超时盖掉。
        let Some(source) = self.pick_source(cx, &req) else {
            return;
        };
        let name = std::path::Path::new(&source)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("upload.gcode")
            .to_string();

        // 命令行点名了另一台设备：把请求放回队列，异步切过去，连上后会自动
        // 再取一次。这里**不删副本、不判失败**——设备没连上不等于交接失败。
        let switch_to = req
            .host
            .clone()
            .filter(|h| !h.is_empty() && h.as_str() != self.host.as_str());
        if let Some(host) = switch_to {
            self.remote_queue.insert(0, req);
            self.connect_host(cx, &host, "");
            return;
        }

        // 命令行进程已经写回过文件了，别在这里再写一次（会多一次备份/写盘）。
        // 材料和 rewrite 都是「这次请求的」参数，用完就还回去，免得影响用户之后
        // 手动拖进来的文件（材料名写错会直接烧到头部里）。
        let rewrite = self.cli.rewrite;
        let material = self.cli.material.take();
        self.cli.rewrite = false;
        self.cli.material = req.material.clone();

        self.set_message(
            cx,
            &format!(
                "命令行交来 {name}，开始上传（{}）",
                if req.start {
                    "传完直接开打"
                } else {
                    "传完再问你"
                }
            ),
        );
        let intent = if req.start {
            Intent::UploadPrint(source)
        } else {
            Intent::UploadOnly(source)
        };
        self.execute(cx, intent);

        self.cli.rewrite = rewrite;
        self.cli.material = material;
    }

    /// 挑一个真能读到的源文件：优先交接副本，退到切片器给的原始路径。
    ///
    /// 两条都读不到才报错。报错里带完整路径和系统错误码 —— 「读取文件失败」这四个字
    /// 对排查毫无用处，得看见到底是 `os error 2`（没了）还是 `os error 32`（被独占）。
    fn pick_source(&mut self, cx: &mut Cx, req: &ipc::Request) -> Option<String> {
        let mut first_err: Option<(String, std::io::Error)> = None;
        for cand in std::iter::once(&req.path).chain(req.origin.iter()) {
            match std::fs::File::open(cand) {
                Ok(_) => {
                    // 副本目录在上传收尾时删掉，别在 %TEMP% 里攒几十 MB 的垃圾。
                    let p = std::path::Path::new(cand);
                    if p.starts_with(crate::handoff::staging_root()) {
                        self.staged_dir = p.parent().map(|d| d.to_path_buf());
                    }
                    return Some(cand.clone());
                }
                Err(e) => {
                    eprintln!("[remote] 读不到 {cand}：{e}");
                    if first_err.is_none() {
                        first_err = Some((cand.clone(), e));
                    }
                }
            }
        }
        if let Some((p, e)) = first_err {
            self.set_message(cx, &format!("读取文件失败：{p}（{e}）"));
            self.sync(cx);
        }
        None
    }

    /// 上传结束（成功或失败）后删掉交接副本。
    fn cleanup_staged(&mut self) {
        if let Some(dir) = self.staged_dir.take() {
            if dir.starts_with(crate::handoff::staging_root()) {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
}

/// 人类可读的体积。
fn kb(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// 显式硬上限（`SF_MAX_UPLOAD`，支持 `3M` / `2500K` / 纯字节）。没设就没有硬上限。
fn upload_limit() -> Option<usize> {
    let raw = std::env::var("SF_MAX_UPLOAD").ok()?;
    let s = raw.trim().to_ascii_lowercase();
    let (num, mul) = if let Some(v) = s.strip_suffix("mb") {
        (v.trim(), 1024 * 1024)
    } else if let Some(v) = s.strip_suffix('m') {
        (v.trim(), 1024 * 1024)
    } else if let Some(v) = s.strip_suffix("kb") {
        (v.trim(), 1024)
    } else if let Some(v) = s.strip_suffix('k') {
        (v.trim(), 1024)
    } else {
        (s.as_str(), 1)
    };
    num.parse::<f64>().ok().map(|v| (v * mul as f64) as usize)
}

/// 体积偏大时给用户的耗时预估（不是拦截，也不再是「可能失败」的警告）。
///
/// 设备侧是「一片 20 KB 等一个 ACK」的锁步节奏，实测 ~0.78 MB/s，
/// 所以 7 MB 的文件要 ~9 秒——先说一声，免得用户以为卡住了。
fn size_note(total: usize) -> Option<String> {
    if total < 2 * 1024 * 1024 {
        return None;
    }
    Some(format!(
        "（设备侧约 {:.1} MB/s，预计需 {:.0} 秒）",
        UPLOAD_RATE_BPS / (1024.0 * 1024.0),
        total as f64 / UPLOAD_RATE_BPS
    ))
}

/// 状态栏只留一行。
///
/// 状态栏高度固定 38px，长报错换行会顶出去（现场撞到过）。
/// 这里按「中文 1 个单位、ASCII 约 0.55 个单位」估算宽度截断，
/// 超过预算就砍掉尾巴加省略号。
fn fit_status(msg: &str) -> String {
    /// 大约能放下 70 个中文字（11.5px 字号，横跨到「急停」按钮前）。
    const BUDGET: f64 = 70.0;
    let mut used = 0.0;
    let mut out = String::new();
    for ch in msg.chars() {
        let w = if (ch as u32) < 0x2E80 { 0.55 } else { 1.0 };
        if used + w > BUDGET {
            out.push('…');
            return out;
        }
        used += w;
        out.push(ch);
    }
    out
}

/// 把文件名收拾成设备能安全处理的形状。
///
/// `9/1` / `11/1` 里的文件名是**裸字节**（无长度前缀，靠 payload 结尾或 NUL 界定），
/// 固件按单字节处理；中文名会写成 UTF-8 多字节串，到了设备侧就是乱码或截断，
/// 之后 `print_start` 再按我们这边的名字去找就对不上了。
///
/// 规则：非 ASCII 可打印字符 → `_`，连续 `_` 合并，长度上限 64（含扩展名）。
/// 返回值第二项表示是否改动过（界面上要告诉用户）。
fn sanitize_name(name: &str) -> (String, bool) {
    const MAX: usize = 64;
    let mut out = String::with_capacity(name.len());
    let mut changed = false;
    for ch in name.chars() {
        let ok = ch.is_ascii() && !ch.is_ascii_control() && ch != '/' && ch != '\\';
        let mapped = if ok { ch } else { '_' };
        if mapped != ch {
            changed = true;
        }
        if mapped == '_' && out.ends_with('_') {
            continue;
        }
        out.push(mapped);
    }
    let out = out.trim_matches('_').to_string();
    // 点开头在设备上等于「隐藏文件」：列表接口（cmd 4 一类的枚举）不会把它列出来，
    // 于是文件传上去了却看不见、也选不到去打印 —— 等于白传。
    // 切片器的中间文件正好就是 `.11596.1.gcode` 这种形状，所以点前缀一律去掉。
    let out = out.trim_start_matches('.').to_string();
    if out.is_empty() {
        return ("upload.gcode".to_string(), true);
    }
    let changed = changed || out != name;
    if out.chars().count() > MAX {
        // 保留扩展名，掐掉中间：滑动器生成的名字常常是「模型名_材料_时间」，尾巴更有用。
        let chars: Vec<char> = out.chars().collect();
        let ext = out.rsplit('.').next().unwrap_or("");
        let tail_len = (ext.chars().count() + 1).min(12);
        let head_len = MAX.saturating_sub(tail_len + 1);
        let head: String = chars[..head_len].iter().collect();
        let tail: String = chars[chars.len() - tail_len..].iter().collect();
        return (format!("{head}_{tail}"), true);
    }
    (out, changed)
}

fn fmt_step(v: f64) -> String {
    if (v - v.round()).abs() < f64::EPSILON {
        format!("{:.0}", v)
    } else {
        format!("{v}")
    }
}

/// 文件名过长时截断，保留扩展名，避免撑破固定高度的行。
fn elide_name(name: &str, max: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max {
        return name.to_string();
    }
    let tail = 10.min(chars.len());
    let head_len = max.saturating_sub(tail + 1);
    let head: String = chars[..head_len].iter().collect();
    let tail_s: String = chars[chars.len() - tail..].iter().collect();
    format!("{head}…{tail_s}")
}

// ───────────────────────────── 事件 ─────────────────────────────

impl MatchEvent for App {
    fn handle_startup(&mut self, cx: &mut Cx) {
        // 之前漏了这一步：`#[rust] cli` 只会是 Default，命令行参数从未生效。
        self.cli = Cli::parse();
        self.confirm_enabled = true;
        self.step_idx = 1; // 默认 1mm
        self.tgt_hotend = 200.0;
        self.tgt_bed = 60.0;
        self.tgt_fan = 0.0;
        self.timer = cx.start_interval(0.5);
        // 上传用 12ms 的快节奏 tick：每个 tick 只做「非阻塞收包 + 最多发一片」，
        // 几微秒就返回，界面不会被拖住（以前一个 tick 占 70ms，窗口连重绘都排不上）。
        // 12ms/片 → 上限 1.7 MB/s，远高于设备实测的 0.78 MB/s，不会成为瓶颈。
        self.upload_timer = cx.start_interval(0.012);

        // 上次运行上传过的文件的切片器总时长。有它，「开始打印」同一份机内文件时
        // 剩余时间就是准的（否则只能退回实测外推）。
        self.file_times = Self::load_file_times();

        // 字体最先装：Makepad 的字体是「按编译期绝对路径读文件」的，
        // 换机器/单文件分发时那些路径不存在 → 全部文字变方块。
        // 这里把内嵌字面填回资源表，后续取字体字节时就用它。
        if crate::fonts::install(cx) == 0 {
            // 脚本资源表可能还没求值，交给定时器再试几次。
            self.font_tries = 0;
        } else {
            self.font_tries = u8::MAX;
        }

        if self.cli.is_headless() {
            self.run_headless();
            // `--status` / `--list` 是给脚本用的：打印完就退出，
            // 否则 Makepad 还会建窗口、跑事件循环，命令永远不返回。
            std::process::exit(0);
        }

        // 常驻本地服务：命令行（Orca 后处理）把「传这个文件」交到这里。
        // 端口被占 = 已经有一个界面在跑，本进程只是用户多开的那一份，
        // 记一条日志继续跑就行（手动操作仍然可用，只是收不到交接）。
        if let Err(e) = ipc::spawn_server() {
            eprintln!(
                "[ipc] 本地服务没起来（端口 {} 已被占用？）：{e}",
                ipc::port()
            );
        }

        // 先放已知设备（上次成功连接过的），再扫局域网补充。
        self.devices = Self::load_known_devices();

        if let Some(host) = self.cli.host.clone() {
            self.connect_host(cx, &host, "");
        } else {
            // 后台扫描，结果回来后再连第一台（见 `poll_discover`）。
            // 启动阶段不做任何同步网络 IO，窗口从第一帧起就能响应。
            self.start_discover(cx, true);
        }
        self.sync(cx);

        // 带文件的调用（Orca 后处理）不会再走到这里：`handoff` 在窗口建起来之前
        // 就把后处理做完、把「要上传」通过本地服务交给了本进程，然后自己退出了。
        // 界面这边只负责在定时器里接活（`drain_remote`）。
    }

    fn handle_timer(&mut self, cx: &mut Cx, event: &TimerEvent) {
        // 后台连接 / 扫描的结果：两个 timer 都会经过这里，每次都收一次。
        // 主线程只做 try_recv，绝不等网络。
        self.poll_connect(cx);
        self.poll_discover(cx);

        // 上传中的快节奏 tick：非阻塞推进一片就返回，界面才画得出进度。
        if self.upload_timer.is_timer(event).is_some() {
            // 命令行交来的任务：用 12ms 的快 tick 收，Orca 的后处理进程
            // 才不用陪着等；真正执行仍要等手上没有上传在跑。
            self.drain_remote(cx);
            if self.upload_job.is_some() {
                self.upload_step(cx);
            }
            return;
        }
        if self.timer.is_timer(event).is_some() {
            // 内嵌字体的重试：脚本资源表若是晚于 startup 才建好，这里补上。
            // 只在读数还是 0 时重试十几次（约 5 秒），装上了就把计数打满。
            if self.font_tries < 10 {
                if crate::fonts::install(cx) > 0 {
                    self.font_tries = u8::MAX;
                } else {
                    self.font_tries += 1;
                }
            }
            // 上传期间不要 drain 收包：ACK 会被这里吃掉，上传就卡死了。
            if self.upload_job.is_some() {
                return;
            }
            if self.conn.is_some() {
                self.poll(cx, 20);
            }
            // 上传完成后的补刷：放在这里做，上传路径上就没有阻塞调用了。
            if std::mem::take(&mut self.refresh_after_upload) && self.conn.is_some() {
                self.refresh_files_after_upload(cx);
            }
        }
    }

    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions) {
        // ── 顶栏：二次确认开关 ──
        let uid = self.ui.check_box(cx, &[id!(confirm_toggle)]).widget_uid();
        if let Some(a) = actions.find_widget_action(uid) {
            if let CheckBoxAction::Change(v) = a.cast() {
                self.confirm_enabled = v;
            }
        }

        // ── 设备：每行「连接」/「断开」（同一行只露一个） ──
        for i in 0..DEV_ROWS {
            if self.ui.button(cx, &[dev_connect_id(i)]).clicked(actions) {
                self.devices_snapshot_guard();
                self.execute(cx, Intent::Connect(i));
                break;
            }
            if self.ui.button(cx, &[dev_disconn_id(i)]).clicked(actions) {
                self.execute(cx, Intent::Disconnect);
                break;
            }
        }

        // ── 重新扫描 ──
        if self.ui.button(cx, &[id!(rescan_btn)]).clicked(actions) {
            self.discover(cx);
        }

        // ── 文件：每行打印 / 删除 ──
        for i in 0..FILE_ROWS {
            let name = match self.files.get(i) {
                Some(n) => n.clone(),
                None => continue,
            };
            if self.ui.button(cx, &[file_print_id(i)]).clicked(actions) {
                self.dispatch(cx, Intent::Print(name.clone()));
                break;
            }
            if self.ui.button(cx, &[file_del_id(i)]).clicked(actions) {
                self.dispatch(cx, Intent::Delete(name.clone()));
                break;
            }
        }

        // ── 文件刷新 ──
        if self
            .ui
            .button(cx, &[id!(file_refresh_btn)])
            .clicked(actions)
        {
            // 上传期间不许刷列表：分页握手会把上传的 9/2 ACK 当无关帧吃掉，
            // 而且一次要占住主线程好几秒。
            if self.upload_job.is_some() {
                self.set_message(cx, "上传进行中，传完再刷新");
            } else {
                self.refresh_files(cx);
            }
        }

        // ── 拖放上传区：拖入文件 → 预览；点击目标 → 系统文件选择框 ──
        let zone = self.ui.dropzone(cx, &[id!(upload_zone)]);
        if let Some(files) = zone.dropped(actions) {
            if let Some(f) = files.first() {
                match self.drop_to_path(f) {
                    Some(path) => self.open_preview(cx, &path),
                    None => self.set_message(cx, "读不到拖入的文件：既没有路径也没有内容"),
                }
            }
        }
        if let Some(files) = zone.refused(actions) {
            let n = files
                .first()
                .map(|f| f.name.clone())
                .unwrap_or_else(|| "未知文件".to_string());
            self.set_message(cx, &format!("只接受 .gcode，已忽略：{n}"));
        }
        if zone.browse(actions) {
            self.pick_upload_file(cx);
        }

        // ── 系统文件选择框的回包（异步，走 Actions） ──
        for action in actions {
            if let Some(picked) = action.downcast_ref::<FileDialogAction>() {
                if picked.id() == PICK_GCODE {
                    if let Some(p) = picked.path() {
                        let p = p.to_string_lossy().into_owned();
                        self.open_preview(cx, &p);
                    }
                }
            }
        }

        // ── 点动：步长 ──
        if self.ui.button(cx, &[id!(step_minus)]).clicked(actions) {
            self.step_idx = self.step_idx.saturating_sub(1);
            self.sync(cx);
        }
        if self.ui.button(cx, &[id!(step_plus)]).clicked(actions) {
            self.step_idx = (self.step_idx + 1).min(STEPS.len() - 1);
            self.sync(cx);
        }

        // ── 点动：六向 ──
        for (id, axis, dir) in [
            (id!(x_minus), 0u8, -1i32),
            (id!(x_plus), 0u8, 1),
            (id!(y_minus), 1u8, -1),
            (id!(y_plus), 1u8, 1),
            (id!(z_minus), 2u8, -1),
            (id!(z_plus), 2u8, 1),
        ] {
            if self.ui.button(cx, &[id]).clicked(actions) {
                self.execute(cx, Intent::Jog(axis, dir));
                break;
            }
        }

        // ── 回原点 ──
        if self.ui.button(cx, &[id!(home_btn)]).clicked(actions) {
            self.dispatch(cx, Intent::Home);
        }

        // ── 挤出 / 回抽 / 释放电机 ──
        if self.ui.button(cx, &[id!(extrude_btn)]).clicked(actions) {
            self.dispatch(cx, Intent::Extrude(5000));
        }
        if self.ui.button(cx, &[id!(retract_btn)]).clicked(actions) {
            self.dispatch(cx, Intent::Extrude(-5000));
        }
        if self.ui.button(cx, &[id!(motor_btn)]).clicked(actions) {
            self.dispatch(cx, Intent::MotorOff);
        }

        // ── 温度 / 风扇步进 ──
        let mut env_changed = false;
        if self.ui.button(cx, &[id!(hotend_minus)]).clicked(actions) {
            self.tgt_hotend = (self.tgt_hotend - 5.0).clamp(0.0, 300.0);
            env_changed = true;
        }
        if self.ui.button(cx, &[id!(hotend_plus)]).clicked(actions) {
            self.tgt_hotend = (self.tgt_hotend + 5.0).clamp(0.0, 300.0);
            env_changed = true;
        }
        if self.ui.button(cx, &[id!(bed_minus)]).clicked(actions) {
            self.tgt_bed = (self.tgt_bed - 5.0).clamp(0.0, 120.0);
            env_changed = true;
        }
        if self.ui.button(cx, &[id!(bed_plus)]).clicked(actions) {
            self.tgt_bed = (self.tgt_bed + 5.0).clamp(0.0, 120.0);
            env_changed = true;
        }
        if self.ui.button(cx, &[id!(fan_minus)]).clicked(actions) {
            self.tgt_fan = (self.tgt_fan - 5.0).clamp(0.0, 100.0);
            env_changed = true;
        }
        if self.ui.button(cx, &[id!(fan_plus)]).clicked(actions) {
            self.tgt_fan = (self.tgt_fan + 5.0).clamp(0.0, 100.0);
            env_changed = true;
        }

        // ── 材料预设 ──
        if self.ui.button(cx, &[id!(pla_btn)]).clicked(actions) {
            self.tgt_hotend = 200.0;
            self.tgt_bed = 60.0;
            env_changed = true;
        }
        if self.ui.button(cx, &[id!(petg_btn)]).clicked(actions) {
            self.tgt_hotend = 230.0;
            self.tgt_bed = 80.0;
            env_changed = true;
        }
        if self.ui.button(cx, &[id!(cool_btn)]).clicked(actions) {
            self.tgt_hotend = 0.0;
            self.tgt_bed = 0.0;
            env_changed = true;
        }
        if env_changed {
            self.sync(cx);
        }

        // ── 应用温度 / 风扇 ──
        if self.ui.button(cx, &[id!(apply_btn)]).clicked(actions) {
            self.execute(cx, Intent::ApplyEnv);
        }

        // ── 急停（立即下发，不弹确认） ──
        if self.ui.button(cx, &[id!(estop_btn)]).clicked(actions) {
            if let Some(c) = &self.conn {
                let _ = c.control_estop();
            }
            self.set_message(cx, "已发送急停（请检查设备）");
        }

        // ── 确认弹窗 ──
        if self.ui.button(cx, &[id!(confirm_ok)]).clicked(actions) {
            self.ui.modal(cx, &[id!(confirm_modal)]).close(cx);
            if let Some(p) = self.pending.take() {
                self.execute(cx, p);
            }
        }
        if self.ui.button(cx, &[id!(confirm_no)]).clicked(actions) {
            self.pending = None;
            self.ui.modal(cx, &[id!(confirm_modal)]).close(cx);
            self.set_message(cx, "已取消");
        }

        // ── 预览弹窗 / 上传完成后的「是否开始打印」 ──
        if self.ui.button(cx, &[id!(preview_print)]).clicked(actions) {
            self.ui.modal(cx, &[id!(preview_modal)]).close(cx);
            let mode = self.preview_mode;
            self.preview_mode = PreviewMode::UploadAndPrint;
            if let Some(p) = self.pending_upload.take() {
                match mode {
                    PreviewMode::UploadAndPrint => self.execute(cx, Intent::UploadPrint(p)),
                    PreviewMode::StartOnly => self.execute(cx, Intent::Print(p)),
                }
            }
        }
        if self.ui.button(cx, &[id!(preview_cancel)]).clicked(actions) {
            self.ui.modal(cx, &[id!(preview_modal)]).close(cx);
            let was_ask = self.preview_mode == PreviewMode::StartOnly;
            self.preview_mode = PreviewMode::UploadAndPrint;
            if was_ask {
                self.set_message(cx, "文件已上传，未开始打印");
            } else {
                self.set_message(cx, "已取消上传");
            }
        }
    }
}

impl App {
    /// 占位：连接前清理旧状态（保留设备列表）。
    fn devices_snapshot_guard(&mut self) {
        self.files.clear();
    }

    /// 打开系统的文件选择框（拖放区被点击时）。
    ///
    /// 结果不会从这里返回：原生对话框跑在别的线程，回包落在
    /// `FileDialogAction` 里，由 `handle_actions` 接。
    fn pick_upload_file(&mut self, cx: &mut Cx) {
        let dialog = FileDialog::new()
            .set_id(PICK_GCODE)
            .set_title("选择要上传的 G-code".to_string())
            .add_filter("G-code".to_string(), vec!["gcode".to_string()])
            .add_filter("所有文件".to_string(), vec!["*".to_string()]);
        cx.open_select_file_dialog(dialog);
    }

    /// 把一次拖放落成磁盘上的路径。
    ///
    /// 桌面拖放自带路径；浏览器拖放只有名字和字节，就先落到临时目录再走同一条路。
    fn drop_to_path(&self, f: &OfferedFile) -> Option<String> {
        if !f.path.is_empty() {
            return Some(f.path.clone());
        }
        let bytes = f.bytes.as_ref()?;
        let dir = std::env::temp_dir().join("starfish-control");
        std::fs::create_dir_all(&dir).ok()?;
        let name = if f.name.is_empty() {
            "upload.gcode"
        } else {
            f.name.as_str()
        };
        let path = dir.join(name);
        std::fs::write(&path, bytes).ok()?;
        Some(path.to_string_lossy().into_owned())
    }

    /// 打开上传预览弹窗。
    ///
    /// 预览里展示的就是**后处理之后**要上传的那份内容：元信息来自重写后的头部，
    /// 缩略图是从 G-code 里挑出来的最大那张。确认后上传的也是同一份。
    fn open_preview(&mut self, cx: &mut Cx, path: &str) {
        let raw = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.set_message(cx, &format!("读取文件失败：{e}"));
                return;
            }
        };
        let text = String::from_utf8_lossy(&raw);
        let converted = header::convert(&text, self.cli.material.as_deref());
        let info = converted.info.clone();
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("upload.gcode")
            .to_string();

        self.pending_upload = Some(path.to_string());
        self.preview_mode = PreviewMode::UploadAndPrint;
        self.ui
            .label(cx, &[id!(preview_title)])
            .set_text(cx, "打印预览");
        self.ui
            .label(cx, &[id!(preview_name)])
            .set_text(cx, &format!("{name}  ·  {}", info.summary()));
        let bytes_len = converted.output.len();
        self.ui.label(cx, &[id!(preview_info)]).set_text(
            cx,
            &format!(
                "共 {:.1} 万行 · 上传 {} · 缩略图 {}",
                info.line_count as f64 / 10000.0,
                kb(bytes_len),
                match &info.thumbnail {
                    Some(t) => format!("{}×{}", t.width, t.height),
                    None => "无".to_string(),
                }
            ),
        );
        let note = if !self.connected {
            "注意：当前未连接设备，开始打印会失败。".to_string()
        } else if let Some(n) = size_note(bytes_len) {
            format!("写入机内存储{n}，然后立即开始打印。")
        } else {
            "确认后会写入机内存储并立即开始打印。".to_string()
        };
        self.ui.label(cx, &[id!(preview_note)]).set_text(cx, &note);
        self.ui
            .button(cx, &[id!(preview_print)])
            .set_text(cx, "上传并开始打印");

        self.show_thumb(cx, info.thumbnail.as_ref());
        self.last_info = Some(info);
        self.ui.modal(cx, &[id!(preview_modal)]).open(cx);
    }

    /// 把缩略图塞进预览弹窗；没有图就把整块藏起来（不留空白）。
    fn show_thumb(&mut self, cx: &mut Cx, thumb: Option<&header::Thumbnail>) {
        match thumb {
            Some(t) => {
                let _ = self
                    .ui
                    .image(cx, &[id!(preview_thumb)])
                    .load_png_from_data(cx, &t.png);
                self.ui
                    .view(cx, &[id!(preview_thumb_box)])
                    .set_visible(cx, true);
            }
            None => {
                self.ui
                    .view(cx, &[id!(preview_thumb_box)])
                    .set_visible(cx, false);
            }
        }
    }

    /// 「重新扫描」按钮：只扫，不自动连接。
    fn discover(&mut self, cx: &mut Cx) {
        self.start_discover(cx, false);
    }

    /// UDP 发现设备并刷新设备行池，**后台执行**。
    ///
    /// 同步版本会让主线程整整 1.2 秒不重绘；设备没开机时我们还会周期性重扫，
    /// 那样界面就会一顿一顿的。结果在 `poll_discover` 里收。
    ///
    /// **不清空**已有列表：UDP 发现常被占用/静默，清空会让用户刚看到的设备消失。
    fn start_discover(&mut self, cx: &mut Cx, auto_connect: bool) {
        if self.discover_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("starfish-discover".into())
            .spawn(move || {
                let r =
                    crate::protocol::connection::discover(std::time::Duration::from_millis(1200));
                let _ = tx.send(r);
            });
        if spawned.is_err() {
            self.set_message(cx, "无法启动扫描线程");
            return;
        }
        self.discover_rx = Some(rx);
        if auto_connect {
            self.auto_connect_after_discover = true;
        }
        self.set_message(cx, "正在扫描局域网…");
        self.sync(cx);
    }

    fn poll_discover(&mut self, cx: &mut Cx) {
        let res = match self.discover_rx.as_ref() {
            Some(rx) => rx.try_recv(),
            None => return,
        };
        let outcome = match res {
            Ok(o) => o,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.discover_rx = None;
                return;
            }
        };
        self.discover_rx = None;
        let auto_connect = std::mem::take(&mut self.auto_connect_after_discover);

        match outcome {
            Ok(devs) => {
                let found = devs.len();
                for d in devs {
                    if let Some(exist) = self.devices.iter_mut().find(|x| x.ip == d.ip) {
                        if exist.serial.is_empty() {
                            exist.serial = d.serial;
                        }
                    } else {
                        self.devices.push(d);
                    }
                }
                let total = self.devices.len();
                self.set_message(
                    cx,
                    &if found > 0 {
                        format!("发现 {found} 台设备（列表共 {total} 台），点行内「连接」")
                    } else if total > 0 {
                        format!("本次未扫到新设备，保留 {total} 台已知设备，可直接连接")
                    } else {
                        "未发现设备：确认打印机在线；也可用 --host 指定 IP 启动".to_string()
                    },
                );
            }
            Err(e) => self.set_message(cx, &format!("扫描失败：{e}")),
        }
        self.sync(cx);

        // 只有启动时的那一次扫描会顺手连第一台；手动点「重新扫描」不会。
        if auto_connect && !self.connected && self.connect_rx.is_none() {
            if let Some(d) = self.devices.first().cloned() {
                self.connect_host(cx, &d.ip, &d.serial);
            }
        }
    }
}

impl AppMain for App {
    fn script_mod(vm: &mut ScriptVm) -> ScriptValue {
        makepad_widgets::script_mod(vm);
        self::script_mod(vm)
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        self.match_event(cx, event);
        self.ui.handle_event(cx, event, &mut Scope::empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_target_prefers_the_host_named_by_the_request() {
        let req = |host: Option<&str>| ipc::Request {
            host: host.map(str::to_string),
            ..Default::default()
        };
        let devs = vec![Device {
            ip: "192.168.1.9".into(),
            serial: "S1".into(),
        }];
        // 点名的设备优先，哪怕队列里第一个请求没点名
        assert_eq!(
            pending_connect_target(&[req(None), req(Some("127.0.0.1"))], &devs),
            Some("127.0.0.1".into())
        );
        // 没人点名 → 第一台已知设备
        assert_eq!(
            pending_connect_target(&[req(None)], &devs),
            Some("192.168.1.9".into())
        );
        // 空字符串不算点名
        assert_eq!(
            pending_connect_target(&[req(Some(""))], &devs),
            Some("192.168.1.9".into())
        );
        // 队列空 / 没有已知设备 → 让调用方去扫描
        assert_eq!(pending_connect_target(&[], &devs), None);
        assert_eq!(pending_connect_target(&[req(None)], &[]), None);
    }

    #[test]
    fn device_names_never_start_with_a_dot() {
        // 设备端把 `.` 开头的文件当隐藏文件，列表接口不列出来 —— 传了也看不见。
        // 切片器的中间文件正好是 `.11596.1.gcode` 这种形状，必须去掉点。
        let (name, changed) = sanitize_name(".11596.1.gcode");
        assert_eq!(name, "11596.1.gcode");
        assert!(changed);

        // 全是点就什么都不剩，退到默认名
        assert_eq!(sanitize_name("...").0, "upload.gcode");
        assert_eq!(sanitize_name(".gcode").0, "gcode");
    }

    #[test]
    fn sanitize_name_keeps_clean_ascii_names() {
        let (name, changed) = sanitize_name("Part 1 (16)_PETG_6m4s.gcode");
        assert_eq!(name, "Part 1 (16)_PETG_6m4s.gcode");
        assert!(!changed);
    }

    #[test]
    fn sanitize_name_transliterates_non_ascii() {
        let (name, changed) = sanitize_name("切片 1.gcode");
        assert!(changed);
        assert!(name.is_ascii(), "必须是纯 ASCII：{name}");
        assert!(name.ends_with(".gcode"), "{name}");
    }

    #[test]
    fn slicer_export_name_becomes_a_stamped_device_name() {
        // 环境变量给的 `Clip S1_PETG_43m31s.gcode` 走到界面后的最终形态。
        // 中文导出名会被固件限制转成 ASCII，但时间戳照样加得上。
        for raw in ["Clip S1_PETG_43m31s.gcode", "切片 1_PETG_6m4s.gcode"] {
            let (name, _) = sanitize_name(raw);
            let stamped = crate::naming::stamp(&name, "20261111-133822");
            assert!(
                stamped.ends_with("_20261111-133822.gcode"),
                "{raw} → {stamped}"
            );
            assert!(stamped.len() <= 64, "{raw} → {stamped}");
            assert!(!stamped.starts_with('.'), "{raw} → {stamped}");
            assert!(stamped.is_ascii(), "{raw} → {stamped}");
        }
    }

    #[test]
    fn sanitize_name_caps_length_but_keeps_extension() {
        let long = format!("{}.gcode", "A".repeat(90));
        let (name, changed) = sanitize_name(&long);
        assert!(changed);
        assert!(name.chars().count() <= 64, "长度 {}", name.chars().count());
        assert!(name.ends_with(".gcode"), "{name}");
    }
}
