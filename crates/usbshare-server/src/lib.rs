//! usbshare server library: exports USB devices over the USB/IP protocol.

pub mod device;
pub mod mock;
pub mod server;
pub mod storage;
#[cfg(target_os = "linux")]
pub mod usbfs;
#[cfg(target_os = "linux")]
pub mod usbfs_raw;
pub mod web;

pub use server::Server;
