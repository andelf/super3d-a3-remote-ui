//! 设备状态解析（cmd 8 / sub 0 的 39 字节响应）。
//!
//! payload 布局（含 4 字节前缀 `[cmd, sub, 0, clientId]`）：
//! ```text
//! [0] cmd=8  [1] sub=0  [2] 0  [3] clientId
//! [4]  printStatus
//! [5]  scrPrintState
//! [6..8]  hotendTemp    int16 LE（当前喷头温度，0.1℃？以设备为准）
//! [8..10] hotendTarget  int16 LE（目标喷头温度）
//! [10..12] bedTemp      int16 LE
//! [12..14] bedTarget    int16 LE
//! [14..18] posX         i32 LE（0.001mm）
//! [18..22] posY
//! [22..26] posZ
//! [26..30] posE
//! [30..32] progress     u16 LE（实际进度 = 值/10，%）
//! [32..38] fanSpeed     u48 LE（6 字节）
//! [38] endStop
//! ```
//! 注：Super3D 源码对 hotendTarget 有运算优先级 bug（`t[8] & (255 + (t[9]<<8))`），
//! 此处按正确方式实现。

/// printStatus 枚举（源码 `C` 表，0–18）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum PrintStatus {
    #[default]
    Free = 0,
    Printing = 1,
    Pause = 2,
    Pausing = 3,
    ErrorPause = 4,
    ErrorPausing = 5,
    EndingForPrint = 6,
    PauseResume = 7,
    PowerLossRecovery = 8,
    Processing = 9,
    FilamentPause = 10,
    FilamentPausing = 11,
    ReadFileAbnormalPause = 12,
    ReadFileAbnormalPausing = 13,
    AutoLeveling = 14,
    ManualLeveling = 15,
    Homing = 16,
    ZOffsetMoving = 17,
    PowerOff = 18,
    Unknown(u8),
}

impl PrintStatus {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => PrintStatus::Free,
            1 => PrintStatus::Printing,
            2 => PrintStatus::Pause,
            3 => PrintStatus::Pausing,
            4 => PrintStatus::ErrorPause,
            5 => PrintStatus::ErrorPausing,
            6 => PrintStatus::EndingForPrint,
            7 => PrintStatus::PauseResume,
            8 => PrintStatus::PowerLossRecovery,
            9 => PrintStatus::Processing,
            10 => PrintStatus::FilamentPause,
            11 => PrintStatus::FilamentPausing,
            12 => PrintStatus::ReadFileAbnormalPause,
            13 => PrintStatus::ReadFileAbnormalPausing,
            14 => PrintStatus::AutoLeveling,
            15 => PrintStatus::ManualLeveling,
            16 => PrintStatus::Homing,
            17 => PrintStatus::ZOffsetMoving,
            18 => PrintStatus::PowerOff,
            other => PrintStatus::Unknown(other),
        }
    }

    /// 默认状态：空闲。
    pub fn label(&self) -> &'static str {
        match self {
            PrintStatus::Free => "free",
            PrintStatus::Printing => "printing",
            PrintStatus::Pause => "pause",
            PrintStatus::Pausing => "pausing",
            PrintStatus::ErrorPause => "error_pause",
            PrintStatus::ErrorPausing => "error_pausing",
            PrintStatus::EndingForPrint => "ending_for_print",
            PrintStatus::PauseResume => "pause_resume",
            PrintStatus::PowerLossRecovery => "power_loss_recovery",
            PrintStatus::Processing => "processing",
            PrintStatus::FilamentPause => "filament_pause",
            PrintStatus::FilamentPausing => "filament_pausing",
            PrintStatus::ReadFileAbnormalPause => "read_file_abnormal_pause",
            PrintStatus::ReadFileAbnormalPausing => "read_file_abnormal_pausing",
            PrintStatus::AutoLeveling => "auto_leveling",
            PrintStatus::ManualLeveling => "manual_leveling",
            PrintStatus::Homing => "homing",
            PrintStatus::ZOffsetMoving => "z_offset_moving",
            PrintStatus::PowerOff => "power_off",
            PrintStatus::Unknown(v) => return Box::leak(format!("unknown({v})").into_boxed_str()),
        }
    }
}

/// 39 字节状态帧（cmd 8/0）解析出来的全部字段。
///
/// 协议里有的字段这里都留着（`client_id` / `scr_print_state` / `pos_e` / `end_stop`
/// 界面暂时用不到），一来解析逻辑与报文布局一一对应、便于对照抓包，
/// 二来将来做「挤出机位置」「限位状态」时直接可用。
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct DeviceStatus {
    pub client_id: u8,
    pub print_status: PrintStatus,
    pub scr_print_state: u8,
    pub hotend_temp: i16,
    pub hotend_target: i16,
    pub bed_temp: i16,
    pub bed_target: i16,
    pub pos_x: i32,
    pub pos_y: i32,
    pub pos_z: i32,
    pub pos_e: i32,
    /// 实际进度百分比（值/10）。
    pub progress: f32,
    pub fan_speed: u64,
    pub end_stop: u8,
}

impl DeviceStatus {
    /// 从 cmd 8/0 的 payload 解析。payload 含 4 字节前缀。
    pub fn from_payload(payload: &[u8]) -> Option<DeviceStatus> {
        if payload.len() < 39 {
            return None;
        }
        let le_i16 = |b: &[u8], o: usize| i16::from_le_bytes([b[o], b[o + 1]]);
        let le_u16 = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let le_i32 = |b: &[u8], o: usize| i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let le_u48 = |b: &[u8], o: usize| {
            let mut v: u64 = 0;
            for k in 0..6 {
                v |= (b[o + k] as u64) << (8 * k);
            }
            v
        };
        Some(DeviceStatus {
            client_id: payload[3],
            print_status: PrintStatus::from_u8(payload[4]),
            scr_print_state: payload[5],
            hotend_temp: le_i16(payload, 6),
            hotend_target: le_i16(payload, 8),
            bed_temp: le_i16(payload, 10),
            bed_target: le_i16(payload, 12),
            pos_x: le_i32(payload, 14),
            pos_y: le_i32(payload, 18),
            pos_z: le_i32(payload, 22),
            pos_e: le_i32(payload, 26),
            progress: le_u16(payload, 30) as f32 / 10.0,
            fan_speed: le_u48(payload, 32),
            end_stop: payload[38],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known() {
        // 构造一个 payload：前缀 8,0,0,12；printStatus=16(homing)
        let mut p = vec![8u8, 0, 0, 12, 16, 0];
        p.extend_from_slice(&[200, 0]); // hotend 200
        p.extend_from_slice(&[210, 0]); // target 210
        p.extend_from_slice(&[60, 0]); // bed 60
        p.extend_from_slice(&[60, 0]); // bed target 60
        p.extend_from_slice(&[0, 0, 0, 0]); // x
        p.extend_from_slice(&[100, 0, 0, 0]); // y = 100 (0.001mm -> 0.1mm)
        p.extend_from_slice(&[0, 0, 0, 0]); // z
        p.extend_from_slice(&[0, 0, 0, 0]); // e
        p.extend_from_slice(&500u16.to_le_bytes()); // progress = 500 -> 50.0%
        p.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // fan
        p.push(0); // endstop
        assert_eq!(p.len(), 39);
        let s = DeviceStatus::from_payload(&p).unwrap();
        assert_eq!(s.print_status, PrintStatus::Homing);
        assert_eq!(s.hotend_temp, 200);
        assert_eq!(s.hotend_target, 210);
        assert_eq!(s.bed_temp, 60);
        assert_eq!(s.pos_y, 100);
        assert_eq!(s.progress, 50.0);
    }
}
