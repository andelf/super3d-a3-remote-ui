//! starfish-net 协议层（纯 Rust，不依赖 Makepad，可被 GUI 与 CLI 共用）。

pub mod connection;
pub mod frame;
pub mod gcode;
pub mod header;
pub mod sacp;
pub mod status;

pub use connection::{Connection, Device};
pub use frame::{build, verify, FrameParser, HEADER_LEN};
pub use gcode::GcodeMeta;
pub use header::{convert, Conversion, HeaderInfo, Thumbnail};
pub use status::{DeviceStatus, PrintStatus};
