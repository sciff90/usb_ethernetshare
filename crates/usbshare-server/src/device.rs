//! Abstractions over "something that looks like a USB device".
//!
//! The server speaks USB/IP to the network and forwards URBs to a
//! [`UsbDevice`]. Devices come from a [`Backend`]: either real hardware
//! (Linux usbfs via `nusb`) or software-simulated mock devices.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use usbip_proto::{errno, ExportedDevice};

/// The 8-byte SETUP packet of a control transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    pub fn parse(b: &[u8; 8]) -> Setup {
        Setup {
            request_type: b[0],
            request: b[1],
            value: u16::from_le_bytes([b[2], b[3]]),
            index: u16::from_le_bytes([b[4], b[5]]),
            length: u16::from_le_bytes([b[6], b[7]]),
        }
    }
    pub fn to_bytes(&self) -> [u8; 8] {
        let v = self.value.to_le_bytes();
        let i = self.index.to_le_bytes();
        let l = self.length.to_le_bytes();
        [
            self.request_type,
            self.request,
            v[0],
            v[1],
            i[0],
            i[1],
            l[0],
            l[1],
        ]
    }
    pub fn is_in(&self) -> bool {
        self.request_type & 0x80 != 0
    }
    /// 0 = standard, 1 = class, 2 = vendor.
    pub fn kind(&self) -> u8 {
        (self.request_type >> 5) & 3
    }
    /// 0 = device, 1 = interface, 2 = endpoint, 3 = other.
    pub fn recipient(&self) -> u8 {
        self.request_type & 0x1f
    }
}

/// Standard request codes (USB 2.0 table 9-4).
pub mod req {
    pub const GET_STATUS: u8 = 0;
    pub const CLEAR_FEATURE: u8 = 1;
    pub const SET_FEATURE: u8 = 3;
    pub const SET_ADDRESS: u8 = 5;
    pub const GET_DESCRIPTOR: u8 = 6;
    pub const SET_DESCRIPTOR: u8 = 7;
    pub const GET_CONFIGURATION: u8 = 8;
    pub const SET_CONFIGURATION: u8 = 9;
    pub const GET_INTERFACE: u8 = 10;
    pub const SET_INTERFACE: u8 = 11;
}

/// A transfer request forwarded from the remote host.
#[derive(Debug, Clone)]
pub struct UrbRequest {
    /// Endpoint address including the direction bit (0x80 = IN).
    /// Control transfers on endpoint 0 use `0x00` or `0x80`.
    pub endpoint: u8,
    /// SETUP packet (only meaningful for control transfers).
    pub setup: [u8; 8],
    /// Data to send (OUT transfers).
    pub data: Vec<u8>,
    /// Number of bytes requested (IN transfers).
    pub length: u32,
    /// Linux URB transfer flags (e.g. `URB_ZERO_PACKET`).
    pub transfer_flags: u32,
    /// Isochronous packets (empty = not isochronous). For OUT transfers each packet's
    /// data is `data[offset..offset + length]`.
    pub iso: Vec<IsoRequest>,
}

/// One isochronous packet of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoRequest {
    pub offset: u32,
    pub length: u32,
}

/// Result of one isochronous packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IsoResult {
    pub actual_length: u32,
    /// 0 or a negative Linux errno.
    pub status: i32,
}

impl UrbRequest {
    pub fn is_iso(&self) -> bool {
        !self.iso.is_empty()
    }
    pub fn is_in(&self) -> bool {
        self.endpoint & 0x80 != 0
    }
    pub fn is_control(&self) -> bool {
        self.endpoint & 0x7f == 0
    }
}

/// Result of a transfer. `status` is 0 or a negative Linux errno.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UrbResponse {
    pub status: i32,
    /// IN data. For isochronous IN transfers this is the received bytes of every
    /// packet concatenated (each packet's `actual_length` bytes, in order), which is
    /// how USB/IP sends them.
    pub data: Vec<u8>,
    /// Bytes transferred (for OUT; equals `data.len()` for IN).
    pub actual_length: u32,
    /// Per-packet results of an isochronous transfer (one per request packet).
    pub iso: Vec<IsoResult>,
    /// Frame number the isochronous transfer started in.
    pub start_frame: i32,
}

impl UrbResponse {
    pub fn in_data(data: Vec<u8>) -> Self {
        let n = data.len() as u32;
        UrbResponse {
            status: 0,
            data,
            actual_length: n,
            ..Default::default()
        }
    }
    pub fn out_ok(n: usize) -> Self {
        UrbResponse {
            status: 0,
            data: Vec::new(),
            actual_length: n as u32,
            ..Default::default()
        }
    }
    pub fn error(errno: i32) -> Self {
        UrbResponse {
            status: -errno,
            data: Vec::new(),
            actual_length: 0,
            ..Default::default()
        }
    }
    pub fn stall() -> Self {
        Self::error(errno::EPIPE)
    }
    pub fn cancelled() -> Self {
        Self::error(errno::ECONNRESET)
    }
}

/// The result of [`UsbDevice::enqueue`].
pub type UrbFuture = Pin<Box<dyn Future<Output = UrbResponse> + Send + 'static>>;

/// An opened, exclusively claimed USB device.
#[async_trait]
pub trait UsbDevice: Send + Sync {
    /// Perform one transfer. Implementations must return promptly once
    /// `cancel` fires (the result is then discarded).
    async fn submit(&self, req: UrbRequest, cancel: &CancellationToken) -> UrbResponse;

    /// Queue a transfer without waiting for earlier ones on the same endpoint.
    ///
    /// Devices that can keep several transfers in flight per endpoint (needed for
    /// isochronous streaming) queue the transfer *before returning*, so the order of
    /// calls is the order on the wire, and return a future for the result. The
    /// future must finish promptly once `cancel` fires. Devices that cannot do this
    /// hand the request back, and the server falls back to [`UsbDevice::submit`],
    /// one transfer at a time per endpoint.
    fn enqueue(&self, req: UrbRequest, _cancel: CancellationToken) -> Result<UrbFuture, UrbRequest> {
        Err(req)
    }

    /// Called when the remote host detaches. Restore kernel drivers etc.
    async fn release(&self) {}
}

/// A shareable device as reported by a backend.
#[derive(Debug, Clone)]
pub struct DeviceEntry {
    pub exported: ExportedDevice,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
}

/// A source of shareable devices.
#[async_trait]
pub trait Backend: Send + Sync {
    /// Short name used in logs and listings ("mock", "usbfs").
    fn name(&self) -> &'static str;
    /// Currently available devices.
    async fn list(&self) -> anyhow::Result<Vec<DeviceEntry>>;
    /// Open and claim the device with the given bus id.
    async fn open(&self, busid: &str) -> anyhow::Result<Arc<dyn UsbDevice>>;
}
