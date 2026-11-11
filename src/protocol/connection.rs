//! 连接层：UDP 发现（11221）+ TCP 控制（7777）+ 后台线程异步桥。
//!
//! Makepad 事件循环在主线程，裸 TCP 必须在专用线程做阻塞 I/O。
//! 设计：后台两个线程（读 / 写），通过 `mpsc` 通道与主线程通信；
//! 主线程在 `handle_timer` 里 `recv()` 收包并写回 `mod.state`。
//! 协议层不依赖任何 Makepad widget，因此 CLI 模式可完全复用。

use std::io;
use std::net::{TcpStream, UdpSocket};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::protocol::frame;

/// 发现的设备。
#[derive(Debug, Clone)]
pub struct Device {
    pub ip: String,
    pub serial: String,
}

/// 心跳间隔。设备的会话看门狗是 4.01 秒（实测），留足余量。
const BEAT_EVERY: Duration = Duration::from_millis(700);

/// UDP 广播发现。发送 `****XC G 1234`，接收 `***XC A <serial>`。
pub fn discover(timeout: Duration) -> io::Result<Vec<Device>> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_broadcast(true)?;
    sock.send_to(b"****XC G 1234", "255.255.255.255:11221")?;
    sock.set_read_timeout(Some(timeout))?;

    let mut devices: Vec<Device> = Vec::new();
    let start = std::time::Instant::now();
    let mut buf = [0u8; 1024];
    loop {
        if start.elapsed() > timeout {
            break;
        }
        match sock.recv_from(&mut buf) {
            Ok((n, addr)) => {
                let msg = String::from_utf8_lossy(&buf[..n]);
                // 期望：`***XC A <serial>`
                if msg.starts_with("***XC A") {
                    let serial = msg.split_whitespace().nth(2).unwrap_or("").to_string();
                    if !serial.is_empty() && !devices.iter().any(|d| d.ip == addr.ip().to_string())
                    {
                        devices.push(Device {
                            ip: addr.ip().to_string(),
                            serial,
                        });
                    }
                }
            }
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(_) => break,
        }
    }
    Ok(devices)
}

/// 与单台设备的 TCP 连接。后台读/写线程通过通道桥接。
pub struct Connection {
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
    client_id: Arc<Mutex<u8>>,
    /// 读线程是否已经退出（对端断开 / socket 出错）。用来在等 ACK 时立刻失败，
    /// 而不是干等到超时。
    closed: Arc<std::sync::atomic::AtomicBool>,
    /// 上一次发 `8/0` 的时刻。设备有 **4 秒会话看门狗**（见 `beat`），
    /// 所有长等待都要靠它续命。
    last_beat: Arc<Mutex<std::time::Instant>>,
    /// `wait_ack` / `recv` 顺手截下来的最新状态帧（上传期间界面靠它刷新）。
    status_slot: Arc<Mutex<Option<Vec<u8>>>>,
}

/// TCP 握手的默认超时。
///
/// 裸 `TcpStream::connect` 对不可达地址会走系统的 SYN 重试，Windows 上能挂 20 秒以上。
/// 以前这个调用直接发生在 UI 线程里，界面就是这么「卡死」的。现在连接整体搬到了
/// 后台线程（`app::connect_host`），这里再给一个硬上限，保证那个线程也一定会回来。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

impl Connection {
    pub fn connect(host: &str, port: u16) -> io::Result<Connection> {
        Self::connect_timeout(host, port, CONNECT_TIMEOUT)
    }

    /// 带超时的连接。地址先解析成 `SocketAddr` 再用 `connect_timeout`，
    /// 这样连不上时不会把调用线程拖进内核的 SYN 重试。
    pub fn connect_timeout(host: &str, port: u16, timeout: Duration) -> io::Result<Connection> {
        use std::net::ToSocketAddrs;
        let addr = (host, port).to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("无法解析地址 {host}:{port}"),
            )
        })?;
        let stream = TcpStream::connect_timeout(&addr, timeout)?;
        Self::from_stream(stream)
    }

    /// 把一条已经连上的 TCP 流包装成「后台读/写线程 + 通道桥」。
    fn from_stream(stream: TcpStream) -> io::Result<Connection> {
        let read_stream = stream.try_clone()?;
        let write_stream = stream;

        let (tx_out, rx_out) = mpsc::channel::<Vec<u8>>();
        let (tx_in, rx_in) = mpsc::channel::<Vec<u8>>();
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let closed_rx = closed.clone();
        let closed_tx = closed.clone();

        // 读线程：阻塞读帧，推入 incoming 通道。
        thread::Builder::new()
            .name("starfish-rx".into())
            .spawn(move || {
                let mut r = read_stream;
                let res = frame::read_frames(&mut r, |payload| {
                    if tx_in.send(payload.to_vec()).is_err() {
                        return Err(io::Error::new(io::ErrorKind::Other, "incoming closed"));
                    }
                    Ok(())
                });
                closed_rx.store(true, std::sync::atomic::Ordering::Relaxed);
                // 读线程一死就再也收不到 ACK，必须留下痕迹，否则现场只看到「设备不应答」。
                eprintln!("[rx] reader thread exited: {res:?}");
            })
            .ok();

        // 写线程：从 outgoing 通道取帧写出。
        thread::Builder::new()
            .name("starfish-tx".into())
            .spawn(move || {
                let mut w = write_stream;
                while let Ok(payload) = rx_out.recv() {
                    if let Err(e) = frame::write_frame(&mut w, &payload) {
                        eprintln!("[tx] write failed: {e}");
                        closed_tx.store(true, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                }
            })
            .ok();

        Ok(Connection {
            tx: tx_out,
            rx: rx_in,
            client_id: Arc::new(Mutex::new(0)),
            closed,
            last_beat: Arc::new(Mutex::new(std::time::Instant::now())),
            status_slot: Arc::new(Mutex::new(None)),
        })
    }

    /// 连接是否已断（读/写线程退出）。
    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// ⚠️ 设备的会话看门狗：**4.01 秒**内收不到 `8/0` 就直接关 TCP（实测，见 README）。
    ///
    /// 关键细节：`9/2` 上传分片**不会**给它续命——抓包实测「一直在传 20KB 分片、
    /// 只是不发 8/0」时，设备仍在第 4.01 秒把连接关掉（官方 App 上传期间每秒发一次 8/0）。
    /// 这就是大文件传到 ~3.3 MB（≈4 秒）必挂的真正原因。
    ///
    /// 任何可能超过 4 秒的等待都必须调 `beat()`。
    pub fn beat(&self) {
        // 注意：判断和发帧要分两次拿锁，`status_query` 里会再锁一次（std Mutex 不可重入）。
        let due = self
            .last_beat
            .lock()
            .map(|t| t.elapsed() >= BEAT_EVERY)
            .unwrap_or(false);
        if due {
            let _ = self.status_query();
        }
    }

    /// 取走等待期间顺手存下的最新状态帧（仅 8/0）。
    ///
    /// `wait_ack` 期间收到的状态帧不会丢，但界面目前直接读 `recv` 的返回值，
    /// 这里暂时没人取——留着是为了将来做「等 ACK 时也能刷新温度/进度」。
    #[allow(dead_code)]
    pub fn take_status(&self) -> Option<Vec<u8>> {
        self.status_slot.lock().ok().and_then(|mut s| s.take())
    }

    /// 发送一帧 payload（含 cmd/sub/clientId 前缀）。
    pub fn send(&self, payload: &[u8]) -> io::Result<()> {
        // 只要发出 8/0 就算续过命，不用调用方操心。
        if payload.len() >= 2 && payload[0] == 8 && payload[1] == 0 {
            if let Ok(mut t) = self.last_beat.lock() {
                *t = std::time::Instant::now();
            }
        }
        self.tx
            .send(payload.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"))
    }

    /// 接收一帧 payload（含超时）。会顺带记下设备下发的 clientId。
    pub fn recv(&self, timeout: Duration) -> Option<Vec<u8>> {
        match self.rx.recv_timeout(timeout) {
            Ok(p) => {
                if p.len() > 3 {
                    let mut cid = self.client_id.lock().unwrap();
                    *cid = p[3];
                }
                if p.len() >= 4 && p[0] == 8 && p[1] == 0 {
                    if let Ok(mut s) = self.status_slot.lock() {
                        *s = Some(p.clone());
                    }
                }
                Some(p)
            }
            Err(_) => None,
        }
    }

    pub fn client_id(&self) -> u8 {
        *self.client_id.lock().unwrap()
    }

    /// 等到某条命令的应答（`payload[0]==cmd && payload[1]==sub`）。
    ///
    /// **上传必须走这个**：设备对每个 9/1、9/2 都会回一帧 ACK，
    /// 一口气把所有分片灌进去会把它冲垮（参考实现 starfish.py 也是每片等一次）。
    /// 期间收到的 8/0 状态帧顺手存进 `status_slot`，不当成错误。
    /// 每轮还会 `beat()` 一次：等太久不给设备发 8/0，它会直接断链。
    pub fn wait_ack(&self, cmd: u8, sub: u8, timeout: Duration) -> io::Result<Vec<u8>> {
        let deadline = std::time::Instant::now() + timeout;
        let debug = std::env::var_os("SF_DEBUG_FRAMES").is_some();
        loop {
            if std::time::Instant::now() >= deadline {
                break;
            }
            if self.is_closed() {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "连接已断开"));
            }
            self.beat();
            match self.recv(Duration::from_millis(200)) {
                Some(p) => {
                    if debug {
                        let head: Vec<String> = p.iter().take(8).map(|b| b.to_string()).collect();
                        eprintln!(
                            "[rx] cmd={} sub={} len={} head=[{}]",
                            p.first().copied().unwrap_or(0),
                            p.get(1).copied().unwrap_or(0),
                            p.len(),
                            head.join(",")
                        );
                    }
                    if p.len() > 1 && p[0] == cmd && p[1] == sub {
                        if p.len() >= 5 && p[4] != 0 {
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                format!("设备返回错误码 {} （cmd {cmd}/{sub}）", p[4]),
                            ));
                        }
                        return Ok(p);
                    }
                }
                None => continue,
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("等待 cmd {cmd}/{sub} 应答超时"),
        ))
    }
}

// ───────────────────────── 命令构造（协议全部发送功能） ─────────────────────────
// 所有命令都依赖设备下发的 clientId（连接后由状态帧给出，默认 0 直到收到）。

impl Connection {
    pub fn status_query(&self) -> io::Result<()> {
        self.send(&frame::pkt(8, 0, self.client_id(), &[]))
    }
    pub fn disconnect(&self) -> io::Result<()> {
        self.send(&frame::pkt(8, 1, self.client_id(), &[]))
    }
    // ── 命令面 ──
    //
    // 下面这批命令都按抓包逐字节验证过，但界面还没接按钮（见 README「已知问题」）。
    // 需要时才接线，所以保留实现，各自标 `allow(dead_code)`；不要因为警告就删掉——
    // 删了等于把验证结论丢掉，接线时又得重抓一遍包。
    #[allow(dead_code)]
    pub fn get_printing_filename(&self) -> io::Result<()> {
        self.send(&frame::pkt(8, 2, self.client_id(), &[]))
    }

    pub fn upload_begin(&self, size: u32, name: &str) -> io::Result<()> {
        let mut b = size.to_le_bytes().to_vec();
        b.extend_from_slice(name.as_bytes());
        self.send(&frame::pkt(9, 1, self.client_id(), &b))
    }
    pub fn upload_fragment(&self, size: u32, chunk: &[u8]) -> io::Result<()> {
        let mut b = size.to_le_bytes().to_vec();
        b.extend_from_slice(chunk);
        self.send(&frame::pkt(9, 2, self.client_id(), &b))
    }
    pub fn upload_complete(&self) -> io::Result<()> {
        self.send(&frame::pkt(9, 3, self.client_id(), &[]))
    }
    pub fn upload_stop(&self) -> io::Result<()> {
        self.send(&frame::pkt(9, 4, self.client_id(), &[]))
    }

    pub fn file_list_begin(&self, media: u8) -> io::Result<()> {
        self.send(&frame::pkt(10, 1, self.client_id(), &[media]))
    }
    pub fn file_list_page(&self) -> io::Result<()> {
        self.send(&frame::pkt(10, 2, self.client_id(), &[]))
    }
    pub fn file_list_end(&self) -> io::Result<()> {
        self.send(&frame::pkt(10, 3, self.client_id(), &[]))
    }
    pub fn file_delete(&self, media: u8, name: &str) -> io::Result<()> {
        let mut b = vec![media];
        b.extend_from_slice(name.as_bytes());
        b.push(0);
        self.send(&frame::pkt(10, 4, self.client_id(), &b))
    }

    pub fn print_start(&self, media: u8, name: &str) -> io::Result<()> {
        let mut b = vec![media];
        b.extend_from_slice(name.as_bytes());
        b.push(0);
        self.send(&frame::pkt(11, 1, self.client_id(), &b))
    }
    #[allow(dead_code)]
    pub fn print_stop(&self) -> io::Result<()> {
        self.send(&frame::pkt(11, 2, self.client_id(), &[]))
    }
    #[allow(dead_code)]
    pub fn print_pause(&self) -> io::Result<()> {
        self.send(&frame::pkt(11, 3, self.client_id(), &[]))
    }
    #[allow(dead_code)]
    pub fn print_resume(&self) -> io::Result<()> {
        self.send(&frame::pkt(11, 4, self.client_id(), &[]))
    }

    pub fn control_home(&self) -> io::Result<()> {
        self.send(&frame::pkt(12, 3, self.client_id(), &[]))
    }
    #[allow(dead_code)]
    pub fn control_home_axis(&self, axis: u8) -> io::Result<()> {
        self.send(&frame::pkt(12, 4, self.client_id(), &[axis]))
    }
    pub fn control_fan(&self, index: u8, speed: u8) -> io::Result<()> {
        self.send(&frame::pkt(12, 1, self.client_id(), &[index, speed]))
    }
    pub fn control_temp(&self, index: u8, celsius: u16) -> io::Result<()> {
        let mut b = vec![index];
        b.extend_from_slice(&celsius.to_le_bytes());
        self.send(&frame::pkt(12, 2, self.client_id(), &b))
    }
    pub fn control_move(&self, axis: u8, distance_um: i32) -> io::Result<()> {
        let mut b = vec![axis];
        b.extend_from_slice(&distance_um.to_le_bytes());
        self.send(&frame::pkt(12, 5, self.client_id(), &b))
    }
    pub fn control_motor_off(&self) -> io::Result<()> {
        self.send(&frame::pkt(12, 6, self.client_id(), &[]))
    }
    pub fn control_extrude(&self, index: u8, amount_um: i32) -> io::Result<()> {
        let mut b = vec![index];
        b.extend_from_slice(&amount_um.to_le_bytes());
        self.send(&frame::pkt(12, 7, self.client_id(), &b))
    }
    #[allow(dead_code)]
    pub fn control_retract(&self, index: u8, amount_um: i32) -> io::Result<()> {
        let mut b = vec![index];
        b.extend_from_slice(&amount_um.to_le_bytes());
        self.send(&frame::pkt(12, 8, self.client_id(), &b))
    }
    #[allow(dead_code)]
    pub fn control_stop_stepper(&self) -> io::Result<()> {
        self.send(&frame::pkt(12, 9, self.client_id(), &[]))
    }
    pub fn control_estop(&self) -> io::Result<()> {
        // 急停：无参数，会使机器立即停止。慎用。
        self.send(&frame::pkt(12, 10, self.client_id(), &[]))
    }

    /// 固件升级（13/1）。命令已验证，界面不提供入口——刷错固件会把机器变砖。
    #[allow(dead_code)]
    pub fn firmware_begin(&self, size: u32, name: &str) -> io::Result<()> {
        let mut b = size.to_le_bytes().to_vec();
        b.extend_from_slice(name.as_bytes());
        self.send(&frame::pkt(13, 1, self.client_id(), &b))
    }
}

// ───────────────────────────── 单元测试 ─────────────────────────────
//
// 2026-10-25 那次「大文件传到 3.3 MB 必挂」的根因是设备 **4.01 秒会话看门狗**，
// 光靠真机验证太贵（会占着打印机、还会写 flash）。这里用一个本地假打印机把
// 固件的三条铁律抄下来，让回归测试能在 CI 里跑：
//   1. 连上之后必须先收到 8/0，才下发 clientId；
//   2. 每收到一个 9/2 分片，回一帧 9/2 ACK；
//   3. **超过 watchdog 没收到 8/0 就关掉 TCP**（9/2 不续命）。
// 第 3 条是关键：把 watchdog 缩到 1 秒、把上传拖到 2.5 秒，
// 就能在几秒内复现「不发心跳必被断、发了心跳就能传完」。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::frame;
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    const CID: u8 = 0x7b;

    /// 假打印机在后台跑出来的观测结果。
    struct FakePrinter {
        port: u16,
        beats: Arc<AtomicU32>,
        received: Arc<Mutex<Vec<u8>>>,
        declared: Arc<AtomicU32>,
        tripped: Arc<AtomicBool>,
        finished: Arc<AtomicBool>,
    }

    impl FakePrinter {
        fn beats(&self) -> u32 {
            self.beats.load(Ordering::SeqCst)
        }
        fn tripped(&self) -> bool {
            self.tripped.load(Ordering::SeqCst)
        }
        fn finished(&self) -> bool {
            self.finished.load(Ordering::SeqCst)
        }
        fn declared(&self) -> u32 {
            self.declared.load(Ordering::SeqCst)
        }
        fn received(&self) -> Vec<u8> {
            self.received.lock().unwrap().clone()
        }
    }

    /// 起一台假打印机。`watchdog` = 多久收不到 8/0 就掐线。
    fn spawn_fake_printer(watchdog: Duration) -> FakePrinter {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let beats = Arc::new(AtomicU32::new(0));
        let received = Arc::new(Mutex::new(Vec::new()));
        let declared = Arc::new(AtomicU32::new(0));
        let tripped = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));

        let (b, r, d, t, f) = (
            beats.clone(),
            received.clone(),
            declared.clone(),
            tripped.clone(),
            finished.clone(),
        );

        thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let _ = sock.set_read_timeout(Some(watchdog));
            let mut parser = frame::FrameParser::new();
            let mut buf = [0u8; 16384];

            loop {
                match sock.read(&mut buf) {
                    Ok(0) => break, // 客户端关掉了
                    Ok(n) => parser.feed(&buf[..n]),
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::TimedOut =>
                    {
                        // 看门狗咬：真实设备这时发的是 FIN，这里直接关 socket。
                        t.store(true, Ordering::SeqCst);
                        break;
                    }
                    Err(_) => break,
                }

                for p in parser.drain() {
                    if p.len() < 2 {
                        continue;
                    }
                    let (cmd, sub) = (p[0], p[1]);
                    // 应答帧和请求同 cmd/sub，只带一个结果码（0 = OK）。
                    let ack = |s: u8, extra: &[u8]| -> Vec<u8> { frame::pkt(cmd, s, CID, extra) };
                    match (cmd, sub) {
                        (8, 0) => {
                            b.fetch_add(1, Ordering::SeqCst);
                            // 39 字节状态帧：`DeviceStatus::from_payload` 的门槛。
                            let mut st = vec![8u8, 0, 0, CID];
                            st.resize(39, 0);
                            if frame::write_frame(&mut sock, &st).is_err() {
                                break;
                            }
                        }
                        (9, 1) => {
                            if p.len() >= 8 {
                                let size = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
                                d.store(size, Ordering::SeqCst);
                            }
                            if frame::write_frame(&mut sock, &ack(1, &[])).is_err() {
                                break;
                            }
                        }
                        (9, 2) => {
                            // 分片：`[len u32][data]` 从偏移 4 开始。
                            if p.len() > 8 {
                                r.lock().unwrap().extend_from_slice(&p[8..]);
                            }
                            if frame::write_frame(&mut sock, &ack(2, &[])).is_err() {
                                break;
                            }
                        }
                        (9, 3) => {
                            f.store(true, Ordering::SeqCst);
                            if frame::write_frame(&mut sock, &ack(3, &[])).is_err() {
                                break;
                            }
                        }
                        (9, 4) => {}
                        (10, 1) => {
                            let _ = frame::write_frame(&mut sock, &ack(1, &[0]));
                        }
                        (10, 2) => {
                            // 一页名字：`[len u32][name\0name\0]`
                            let names = b"a.gcode\0b.gcode\0".to_vec();
                            let mut body = (names.len() as u32).to_le_bytes().to_vec();
                            body.extend_from_slice(&names);
                            let _ = frame::write_frame(&mut sock, &ack(2, &body));
                        }
                        (10, 3) => {
                            let _ = frame::write_frame(&mut sock, &ack(3, &[1, 0]));
                        }
                        _ => {}
                    }
                }
            }
        });

        FakePrinter {
            port,
            beats,
            received,
            declared,
            tripped,
            finished,
        }
    }

    fn connect(printer: &FakePrinter) -> Connection {
        let c = Connection::connect("127.0.0.1", printer.port).expect("connect");
        // 握手：clientId 只从设备回包里下发。
        let _ = c.status_query();
        assert!(c.recv(Duration::from_secs(2)).is_some(), "握手没收到状态帧");
        assert_eq!(c.client_id(), CID, "clientId 没记住");
        c
    }

    /// 核心回归：上传耗时远超看门狗，只要有 8/0 续命就必须能传完。
    ///
    /// 这条测试本身就是当时那个 bug 的复现器——把 `beat()` 去掉就会失败。
    #[test]
    fn upload_outlives_the_watchdog_when_beating() {
        let printer = spawn_fake_printer(Duration::from_millis(1000));
        let conn = connect(&printer);

        // 62 KB，1 KB 一片 × 40 ms ≈ 2.5 秒 = 看门狗的 2.5 倍。
        let data: Vec<u8> = (0..62_000u32).map(|i| (i % 251) as u8).collect();
        conn.upload_begin(data.len() as u32, "loopback.gcode")
            .unwrap();
        conn.wait_ack(9, 1, Duration::from_secs(2))
            .expect("9/1 没 ACK");
        assert_eq!(printer.declared(), data.len() as u32, "9/1 声明的长度不对");

        for chunk in data.chunks(1024) {
            conn.beat(); // ← 就是这一句救了命
            thread::sleep(Duration::from_millis(40));
            conn.upload_fragment(chunk.len() as u32, chunk).unwrap();
            conn.wait_ack(9, 2, Duration::from_secs(2))
                .expect("分片 ACK 超时（看门狗咬了？）");
        }

        conn.upload_complete().unwrap();
        conn.wait_ack(9, 3, Duration::from_secs(2))
            .expect("9/3 没 ACK");

        assert!(!printer.tripped(), "设备看门狗还是咬断了连接");
        assert!(printer.finished(), "没收到 9/3");
        assert_eq!(printer.received(), data, "机内收到的字节和发出的不一致");
        assert!(printer.beats() >= 2, "心跳次数太少：{}", printer.beats());
    }

    /// 对照组：不发 8/0 时，假打印机必须把连接掐掉。
    ///
    /// 没有这条，上面那条测试可能只是「假设备根本不会断」，就没有意义了。
    #[test]
    fn idle_link_without_heartbeat_is_killed() {
        let printer = spawn_fake_printer(Duration::from_millis(600));
        let conn = connect(&printer);

        let started = std::time::Instant::now();
        let mut closed = false;
        while started.elapsed() < Duration::from_secs(4) {
            // 只收不发：心跳一次都不打。
            let _ = conn.recv(Duration::from_millis(50));
            if conn.is_closed() {
                closed = true;
                break;
            }
        }
        assert!(closed, "不发心跳居然没被断开，这个假设备不可信");
        assert!(printer.tripped(), "断开不是看门狗干的");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "断开得太晚（{:?}），不像看门狗",
            started.elapsed()
        );
    }

    /// `beat()` 自己就能把空闲连接续住（上传外的长等待也靠它）。
    #[test]
    fn beat_keeps_an_idle_link_alive() {
        let printer = spawn_fake_printer(Duration::from_millis(1000));
        let conn = connect(&printer);

        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_millis(2000) {
            conn.beat();
            let _ = conn.recv(Duration::from_millis(50));
        }
        assert!(!conn.is_closed(), "打了心跳还被断开");
        assert!(printer.beats() >= 2, "心跳发得太少：{}", printer.beats());
    }

    /// 拉文件列表时也要有心跳，否则「上传完 → 刷新列表」会在最后一步超时。
    #[test]
    fn file_list_fetch_beats_during_its_waits() {
        let printer = spawn_fake_printer(Duration::from_millis(800));
        let conn = connect(&printer);

        let started = std::time::Instant::now();
        // 模拟 fetch_file_list 的两轮等待（每轮 1.8s，中途只 recv）。
        for _ in 0..2 {
            let _ = conn.file_list_begin(0);
            let deadline = std::time::Instant::now() + Duration::from_millis(900);
            while std::time::Instant::now() < deadline {
                conn.beat();
                let _ = conn.recv(Duration::from_millis(20));
            }
        }
        assert!(!conn.is_closed(), "拉列表期间连接被看门狗咬断");
        assert!(
            started.elapsed() > Duration::from_millis(1500),
            "循环没跑够时间，测试没验证到 4 秒这个量级"
        );
    }
}
