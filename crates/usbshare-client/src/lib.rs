//! usbshare client library: use USB devices shared by a usbshare (or any USB/IP) server.

pub mod host;
pub mod remote;
pub mod selftest;
#[cfg(target_os = "linux")]
pub mod vhci;
