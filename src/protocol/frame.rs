//! starfish-net 帧编解码。
//!
//! 帧格式（全部小端，已用抓包 178/178 帧验证）：
//! ```text
//! A5 5A | 40 | 长度 uint32LE | XOR(长度4字节) | sum(payload) uint16LE | payload
//! ```
//! 整帧长度 = 10 + 长度。帧尾无校验。
//!
//! 校验：
//! - 偏移 7 = 4 个长度字节的 XOR
//! - 偏移 8..10 = payload 所有字节求和（uint16，截断）
//!
//! payload 通用布局：`[cmd, sub, 0x00, clientId, ...]`。

use std::io::{self, Read, Write};

pub const SOF0: u8 = 0xA5;
pub const SOF1: u8 = 0x5A;
pub const HEADER_FLAG: u8 = 0x40;
pub const HEADER_LEN: usize = 10;

/// 构造一帧。payload 即命令体（含 cmd/sub/clientId 前缀由各命令构造器给出）。
pub fn build(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.push(SOF0);
    frame.push(SOF1);
    frame.push(HEADER_FLAG);
    frame.extend_from_slice(&len.to_le_bytes());
    let xor = payload_len_xor(&len.to_le_bytes());
    frame.push(xor);
    let sum = payload_sum(payload);
    frame.extend_from_slice(&sum.to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// 偏移 7 的校验字节：4 个长度字节的 XOR。
pub fn payload_len_xor(len_bytes: &[u8; 4]) -> u8 {
    len_bytes[0] ^ len_bytes[1] ^ len_bytes[2] ^ len_bytes[3]
}

/// 偏移 8..10 的校验：payload 字节求和（截断到 u16）。
pub fn payload_sum(payload: &[u8]) -> u16 {
    payload.iter().map(|&b| b as u32).sum::<u32>() as u16
}

/// 校验一帧的校验字节是否正确。
pub fn verify(frame: &[u8]) -> bool {
    if frame.len() < HEADER_LEN {
        return false;
    }
    if frame[0] != SOF0 || frame[1] != SOF1 || frame[2] != HEADER_FLAG {
        return false;
    }
    let len = u32::from_le_bytes([frame[3], frame[4], frame[5], frame[6]]) as usize;
    if frame.len() != HEADER_LEN + len {
        return false;
    }
    if frame[7] != payload_len_xor(&[frame[3], frame[4], frame[5], frame[6]]) {
        return false;
    }
    let got_sum = u16::from_le_bytes([frame[8], frame[9]]);
    if got_sum != payload_sum(&frame[HEADER_LEN..]) {
        return false;
    }
    true
}

/// 流式帧解析器：可增量喂入字节，吐出 payload。
pub struct FrameParser {
    buf: Vec<u8>,
}

impl FrameParser {
    pub fn new() -> Self {
        FrameParser { buf: Vec::new() }
    }

    pub fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// 从缓冲中尽可能多地取出完整 payload。不完整的帧留在缓冲。
    pub fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            match Self::next_payload(&self.buf) {
                Some((payload, consumed)) => {
                    out.push(payload);
                    self.buf.drain(..consumed);
                }
                None => break,
            }
        }
        out
    }

    /// 找到下一帧起始并取出 payload。
    fn next_payload(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
        let mut i = 0;
        while i + HEADER_LEN <= buf.len() {
            if buf[i] == SOF0 && buf[i + 1] == SOF1 && buf[i + 2] == HEADER_FLAG {
                let len = u32::from_le_bytes([buf[i + 3], buf[i + 4], buf[i + 5], buf[i + 6]])
                    as usize;
                let end = i + HEADER_LEN + len;
                if end > buf.len() {
                    // 帧未完整到达
                    return None;
                }
                let payload = buf[i + HEADER_LEN..end].to_vec();
                return Some((payload, end));
            }
            i += 1;
        }
        None
    }
}

/// 便捷构造命令 payload：`[cmd, sub, 0, clientId, args...]`。
pub fn pkt(cmd: u8, sub: u8, client_id: u8, args: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + args.len());
    p.push(cmd);
    p.push(sub);
    p.push(0);
    p.push(client_id);
    p.extend_from_slice(args);
    p
}

/// 单帧 payload 上限（超过就认为失同步，滑过去重新找帧头）。
///
/// 上传分片是 20480 字节，状态帧只有几十字节，64KB 足够宽松；
/// 没有上限的话，一个被读错的长度字段会让我们去申请几个 GB 并永久阻塞。
const MAX_PAYLOAD: usize = 64 * 1024;

/// 从可读流里连续读帧并回调（用于后台线程阻塞读 TCP）。
///
/// 用滚动缓冲而不是 `read_exact(header)+read_exact(payload)`：
/// 后者一旦失同步就再也回不来（原来那个「跳 1 字节」的分支其实每次都多读 9 字节），
/// 表现就是上传到一半设备还在发 ACK、我们却再也收不到，然后一直等到超时。
pub fn read_frames<R: Read>(
    reader: &mut R,
    mut on_frame: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let debug = std::env::var_os("SF_DEBUG_FRAMES").is_some();
    let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut chunk = [0u8; 16384];
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            if debug {
                eprintln!("[rx] eof, left={}", buf.len());
            }
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);

        loop {
            // 找帧头
            let mut start = None;
            let limit = buf.len().saturating_sub(2);
            let mut i = 0;
            while i < limit {
                if buf[i] == SOF0 && buf[i + 1] == SOF1 && buf[i + 2] == HEADER_FLAG {
                    start = Some(i);
                    break;
                }
                i += 1;
            }
            match start {
                Some(s) => {
                    if s > 0 {
                        if debug {
                            eprintln!("[rx] resync: dropped {s} bytes");
                        }
                        buf.drain(..s);
                    }
                }
                None => {
                    // 全是垃圾：留最后 2 字节（可能是被切断的帧头），其余丢掉。
                    if buf.len() > 2 {
                        let keep = buf.split_off(buf.len() - 2);
                        if debug {
                            eprintln!("[rx] no sof in {} bytes", buf.len());
                        }
                        buf = keep;
                    }
                    break;
                }
            }

            if buf.len() < HEADER_LEN {
                break;
            }
            let len = u32::from_le_bytes([buf[3], buf[4], buf[5], buf[6]]) as usize;
            if len > MAX_PAYLOAD {
                // 长度离谱：不是真帧头，滑过这 3 个字节继续找。
                if debug {
                    eprintln!("[rx] bogus length {len}, skipping sof");
                }
                buf.drain(..3);
                continue;
            }
            if buf.len() < HEADER_LEN + len {
                break; // 帧没到齐，等更多数据
            }
            let payload = buf[HEADER_LEN..HEADER_LEN + len].to_vec();
            buf.drain(..HEADER_LEN + len);
            on_frame(&payload)?;
        }
    }
}

/// 写一帧到可写流。
pub fn write_frame<W: Write>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    let frame = build(payload);
    writer.write_all(&frame)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for pl in [vec![], vec![8, 0, 0, 12], vec![12, 3, 0, 12], (0u8..=255).collect::<Vec<_>>()] {
            let f = build(&pl);
            assert!(verify(&f), "verify failed for {:?}", pl);
            assert_eq!(f.len(), HEADER_LEN + pl.len());
        }
    }

    #[test]
    fn parse_roundtrip() {
        let mut p = FrameParser::new();
        let mut all = Vec::new();
        for pl in [vec![8, 0, 0, 12], vec![9, 2, 0, 12, 1, 2, 3, 4]] {
            all.extend_from_slice(&build(&pl));
        }
        p.feed(&all);
        let got = p.drain();
        assert_eq!(got, vec![vec![8, 0, 0, 12], vec![9, 2, 0, 12, 1, 2, 3, 4]]);
    }
}
