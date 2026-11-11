//! SACP 协议（Snapmaker 通用协议，魔数 `AA 55`，13 字节头）。
//!
//! **本设备（192.168.0.227）不运行 SACP**：全端口扫描仅开 22(SSH) 与 7777(starfish-net)，
//! SACP 端口不存在；Super3D 的 `src/server/index.js` 也未参与星鱼链路（仅串口 / SACP 工作区）。
//!
//! 此处仅保留魔数与头部结构作为后续扩展参考，不实现收发。
//! 若需要支持其它运行 SACP 的 Snapmaker 机型，再在此补全 Dispatcher / Request / Response。
//!
//! 因为整模块都是「未接线的参考定义」，这里统一放行 dead_code。
#![allow(dead_code)]

pub const SACP_SOF: [u8; 2] = [0xAA, 0x55];
pub const SACP_HEADER_LEN: usize = 13;

/// SACP 头部布局（大端，13 字节），与 starfish-net 不同——不要混淆两者。
pub struct SacpHeader;

impl SacpHeader {
    pub const LEN: usize = SACP_HEADER_LEN;
}
