//! 单实例「全局服务」：命令行进程 ↔ 图形界面进程的一条本地通道。
//!
//! # 为什么需要它
//!
//! OrcaSlicer 的后处理脚本是**同步**调的：脚本不退出，切片器就一直等，界面表现为
//! 「无响应」。旧实现里，带文件启动的那个进程会把「后处理 + 连设备 + 上传 + 弹窗」
//! 一口气全干完，上传要几秒到几十秒（设备只有 0.78 MB/s），切片器就卡那么久；
//! 上传期间还会占着 UI 线程做 `wait_ack`，窗口看起来直接死掉。
//!
//! 现在分工是：
//! - 命令行进程：只做 G-code 后处理（纯本地、毫秒级），把「这个文件要上传」交给界面，
//!   然后**立刻退出**。切片器看到的只是「脚本跑完了」。
//! - 图形界面进程：常驻，开着这个本地服务。收到请求后在自己的事件循环里慢慢传，
//!   传完再问要不要打印。
//!
//! # 为什么是 127.0.0.1 上的固定端口，而不是命名管道
//!
//! Windows 上要拿命名管道要么写 FFI 要么加 `windows-sys` 依赖，而这里只需要
//! 「有没有人在听」+「传一段文本」两件事，std 的 TcpStream 就够。为了不被别的程序
//! 占端口骗到，握手固定带 `STARFISH1` 魔数：对面不认，就当「没有界面在跑」。
//!
//! 端口可以用环境变量 `STARFISH_CTL_PORT` 覆盖（自测/多实例时用）。

use std::io::{self, BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Mutex;
use std::time::Duration;

/// 握手魔数。改它就等于换了协议版本：老界面认不出新命令行，会走「拉起一个新界面」。
const MAGIC: &str = "STARFISH1";
/// 默认端口。选一个不常见的高位端口，尽量不和别人的东西撞。
pub const DEFAULT_PORT: u16 = 51721;
/// 单次读写的超时。这些都是本机回环，几百微秒的事，给 800ms 已经非常宽松。
const IO_TIMEOUT: Duration = Duration::from_millis(800);
/// 探测「有没有界面在跑」的连接超时。回环上没人听会立刻 ConnectionRefused，
/// 这个超时只兜住「防火墙把包吞了」这类异常情况。
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// 服务端口（可用 `STARFISH_CTL_PORT` 覆盖）。
pub fn port() -> u16 {
    std::env::var("STARFISH_CTL_PORT")
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .filter(|p| *p != 0)
        .unwrap_or(DEFAULT_PORT)
}

fn addr() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port()))
}

/// 命令行交给界面的一件事：把这个文件传到设备上。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    /// 要上传的 G-code 绝对路径。空字符串 = 心跳（只探活，不做事）。
    ///
    /// 通常指向命令行进程落在自己临时目录里的**副本**（见 `handoff::stage_copy`）：
    /// 切片器（Orca）在脚本退出后会立刻把导出文件挪走或重开写句柄，界面晚几百
    /// 毫秒再去读原路径就会「读取文件失败」。副本的寿命由我们控制。
    pub path: String,
    /// 原始路径（切片器给的那个），仅用于兜底读取和状态栏显示。
    pub origin: Option<String>,
    /// 后处理用的材料名（`--material`）。界面自己重算头部时要一致。
    pub material: Option<String>,
    /// 命令行指定的目标设备（`--host`）。界面对不上就切过去。
    pub host: Option<String>,
    /// 上传完直接开打（`--yes`），不再弹「是否开始打印」。
    pub start: bool,
}

impl Request {
    fn encode(&self) -> String {
        let mut s = String::with_capacity(96 + self.path.len());
        s.push_str(MAGIC);
        s.push('\n');
        s.push_str("path=");
        s.push_str(&self.path);
        s.push('\n');
        if let Some(o) = &self.origin {
            s.push_str("origin=");
            s.push_str(o);
            s.push('\n');
        }
        if let Some(m) = &self.material {
            s.push_str("material=");
            s.push_str(m);
            s.push('\n');
        }
        if let Some(h) = &self.host {
            s.push_str("host=");
            s.push_str(h);
            s.push('\n');
        }
        if self.start {
            s.push_str("start=1\n");
        }
        // 空行结束：服务端读到空行就停，不必依赖半关闭。
        s.push('\n');
        s
    }

    fn decode(lines: &[String]) -> Request {
        let mut req = Request::default();
        for line in lines {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k {
                "path" => req.path = v.to_string(),
                "origin" => {
                    if !v.is_empty() {
                        req.origin = Some(v.to_string())
                    }
                }
                "material" => {
                    if !v.is_empty() {
                        req.material = Some(v.to_string())
                    }
                }
                "host" => {
                    if !v.is_empty() {
                        req.host = Some(v.to_string())
                    }
                }
                "start" => req.start = v == "1" || v.eq_ignore_ascii_case("true"),
                _ => {}
            }
        }
        req
    }
}

/// 有没有图形界面在跑。问法是发一个「空路径」的请求，对面回 `OK` 才算数。
pub fn is_running() -> bool {
    send(&Request::default()).is_ok()
}

/// 把请求交给正在跑的界面。`Err` = 没人接（或者对面不是我们）。
pub fn send(req: &Request) -> io::Result<String> {
    let mut s = TcpStream::connect_timeout(&addr(), PROBE_TIMEOUT)?;
    s.set_read_timeout(Some(IO_TIMEOUT))?;
    s.set_write_timeout(Some(IO_TIMEOUT))?;
    let payload = req.encode();
    s.write_all(payload.as_bytes())?;
    s.flush()?;

    let mut reader = BufReader::new(s);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "界面没有应答"));
    }
    let line = line.trim_end().to_string();
    match line.strip_prefix("OK ") {
        Some(msg) => Ok(msg.to_string()),
        None => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("对端不是 starfish-control：{line}"),
        )),
    }
}

/// 已收到、等着界面消化的请求。
///
/// 服务线程只负责「收下来 + 排队」；真正的上传必须回到 UI 线程做（连接、弹窗、
/// 进度条都在那儿）。所以中间就放这么一个静态队列，界面在定时器里取。
static QUEUE: Mutex<Vec<Request>> = Mutex::new(Vec::new());

/// 取走所有待处理请求（界面定时器调用）。
pub fn take_requests() -> Vec<Request> {
    match QUEUE.lock() {
        Ok(mut q) => std::mem::take(&mut *q),
        Err(_) => Vec::new(),
    }
}

/// 在后台线程上开监听。**只有图形界面进程该调它**。
///
/// 返回 `Err` 通常意味着端口已被占用——也就是已经有一个界面实例在跑。
pub fn spawn_server() -> io::Result<()> {
    let listener = TcpListener::bind(addr())?;
    std::thread::Builder::new()
        .name("starfish-ipc".into())
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(s) => {
                        if let Err(e) = serve_one(s) {
                            eprintln!("[ipc] 连接处理失败：{e}");
                        }
                    }
                    Err(e) => eprintln!("[ipc] accept 失败：{e}"),
                }
            }
        })?;
    Ok(())
}

fn serve_one(mut s: TcpStream) -> io::Result<()> {
    s.set_read_timeout(Some(IO_TIMEOUT))?;
    s.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(s.try_clone()?);

    let mut first = String::new();
    if reader.read_line(&mut first)? == 0 {
        return Ok(());
    }
    if first.trim_end() != MAGIC {
        return writeln!(s, "ERR 不认识的握手：{}", first.trim_end());
    }

    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if line.is_empty() {
            break;
        }
        lines.push(line);
        if lines.len() > 12 {
            return writeln!(s, "ERR 请求过长");
        }
    }

    let req = Request::decode(&lines);
    if req.path.is_empty() {
        // 探活：不排队，直接答一声。
        return writeln!(s, "OK starfish-control");
    }

    let reply = match QUEUE.lock() {
        Ok(mut q) => {
            q.push(req);
            "已交给界面处理"
        }
        Err(_) => return writeln!(s, "ERR 队列锁坏了"),
    };
    writeln!(s, "OK {reply}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let req = Request {
            path: r"C:\Users\me\切片 1.gcode".into(),
            origin: Some(r"D:\导出\切片 1.gcode".into()),
            material: Some("黑色-PETG".into()),
            host: Some("192.168.0.227".into()),
            start: true,
        };
        let text = req.encode();
        let lines: Vec<String> = text
            .lines()
            .skip(1)
            .map(|s| s.trim_end_matches('\r').to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(Request::decode(&lines), req);
    }

    #[test]
    fn ping_is_an_empty_request() {
        let ping = Request::default();
        assert!(ping.path.is_empty());
        let lines = vec!["path=".to_string()];
        assert_eq!(Request::decode(&lines), ping);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let lines = vec![
            "path=D:\\a.gcode".to_string(),
            "future=whatever".to_string(),
            "start=true".to_string(),
        ];
        let req = Request::decode(&lines);
        assert_eq!(req.path, "D:\\a.gcode");
        assert!(req.start);
        assert_eq!(req.material, None);
    }

    #[test]
    fn port_override_is_honoured() {
        // 这个测试会改进程环境变量，所以只在自己独占的进程里跑（单测默认多线程，
        // 但没别的测试读这个变量，改动范围安全）。
        std::env::set_var("STARFISH_CTL_PORT", "51999");
        assert_eq!(port(), 51999);
        std::env::set_var("STARFISH_CTL_PORT", "not-a-port");
        assert_eq!(port(), DEFAULT_PORT);
        std::env::remove_var("STARFISH_CTL_PORT");
    }
}
