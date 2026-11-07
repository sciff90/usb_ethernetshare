//! Vendor-specific bulk loopback device: whatever is written to EP 0x01 OUT
//! can be read back from EP 0x81 IN.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use usbip_proto::Speed;

use super::{DeviceSpec, EndpointSpec, InterfaceSpec, MockFunction, MOCK_VID, PID_LOOPBACK};
use crate::device::{Setup, UrbRequest, UrbResponse};

const CAPACITY: usize = 1024 * 1024;

/// Vendor request returning the number of buffered bytes (u32 LE).
pub const VENDOR_REQ_FILL_LEVEL: u8 = 0x01;

pub struct LoopbackDevice {
    spec: DeviceSpec,
    buf: Mutex<VecDeque<u8>>,
    readable: Notify,
    writable: Notify,
}

impl LoopbackDevice {
    pub fn new(serial: String) -> Self {
        LoopbackDevice {
            spec: DeviceSpec {
                vid: MOCK_VID,
                pid: PID_LOOPBACK,
                bcd_device: 0x0100,
                speed: Speed::High,
                manufacturer: "usbshare".into(),
                product: "usbshare Loopback".into(),
                serial,
                interfaces: vec![InterfaceSpec {
                    class: 0xff,
                    subclass: 0,
                    protocol: 0,
                    extra: vec![],
                    endpoints: vec![
                        EndpointSpec {
                            address: 0x01,
                            attributes: 2,
                            max_packet: 512,
                            interval: 0,
                        },
                        EndpointSpec {
                            address: 0x81,
                            attributes: 2,
                            max_packet: 512,
                            interval: 0,
                        },
                    ],
                }],
            },
            buf: Mutex::new(VecDeque::new()),
            readable: Notify::new(),
            writable: Notify::new(),
        }
    }
}

#[async_trait]
impl MockFunction for LoopbackDevice {
    fn spec(&self) -> &DeviceSpec {
        &self.spec
    }

    async fn control(&self, setup: Setup, _data: &[u8]) -> Option<UrbResponse> {
        // Vendor, device recipient, IN.
        if setup.request_type == 0xc0 && setup.request == VENDOR_REQ_FILL_LEVEL {
            let n = self.buf.lock().unwrap().len() as u32;
            return Some(UrbResponse::in_data(n.to_le_bytes().to_vec()));
        }
        None
    }

    async fn transfer(&self, req: UrbRequest, cancel: &CancellationToken) -> UrbResponse {
        match req.endpoint {
            0x01 => loop {
                let notified = self.writable.notified();
                {
                    let mut b = self.buf.lock().unwrap();
                    if b.len() + req.data.len() <= CAPACITY || b.is_empty() {
                        b.extend(req.data.iter());
                        drop(b);
                        self.readable.notify_waiters();
                        return UrbResponse::out_ok(req.data.len());
                    }
                }
                tokio::select! {
                    _ = notified => {}
                    _ = cancel.cancelled() => return UrbResponse::cancelled(),
                }
            },
            0x81 => loop {
                let notified = self.readable.notified();
                {
                    let mut b = self.buf.lock().unwrap();
                    if !b.is_empty() {
                        let n = b.len().min(req.length as usize);
                        let out: Vec<u8> = b.drain(..n).collect();
                        drop(b);
                        self.writable.notify_waiters();
                        return UrbResponse::in_data(out);
                    }
                }
                tokio::select! {
                    _ = notified => {}
                    _ = cancel.cancelled() => return UrbResponse::cancelled(),
                }
            },
            _ => UrbResponse::stall(),
        }
    }

    fn reset(&self) {
        self.buf.lock().unwrap().clear();
        self.writable.notify_waiters();
    }
}
