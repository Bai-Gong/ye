//! 硬件加速后端：设备内存与硬件编解码会话的抽象。
//!
//! 计划后端：VideoToolbox（macOS）、D3D11VA / NVENC（Windows）、
//! VA-API（Linux）。软硬路径共享上层 trait，切换对调用方透明。
