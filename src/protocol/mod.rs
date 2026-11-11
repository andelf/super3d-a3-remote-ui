//! starfish-net 协议层（纯 Rust，不依赖 Makepad，可被 GUI 与 CLI 共用）。
//!
//! 按抓包验证的报文逐层拆开：`frame` 管帧、`status` 管状态帧、`connection` 管会话。
//! 调用方一律写全路径（`protocol::status::DeviceStatus`），这里不做 `pub use` 汇总——
//! 汇总出来的名字没人用，只会变成一串 unused import 警告。

pub mod connection;
pub mod frame;
pub mod gcode;
pub mod header;
pub mod sacp;
pub mod status;
