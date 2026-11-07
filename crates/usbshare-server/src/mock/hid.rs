//! A harmless vendor-defined HID device (it is *not* a keyboard or mouse, so
//! attaching it never injects input into the client machine).
//!
//! * Input report (8 bytes, EP 0x81, every 100 ms):
//!   `[counter: u32 LE, last output report bytes 0..4]`
//! * Output report (8 bytes): via EP 0x01 interrupt OUT or SET_REPORT.
//!
//! On a Linux client it binds to `usbhid` and appears as `/dev/hidrawN`.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use usbip_proto::Speed;

use super::{DeviceSpec, EndpointSpec, InterfaceSpec, MockFunction, MOCK_VID, PID_HID};
use crate::device::{Setup, UrbRequest, UrbResponse};

pub const REPORT_INTERVAL: Duration = Duration::from_millis(100);

pub const REPORT_DESCRIPTOR: &[u8] = &[
    0x06, 0x00, 0xff, // Usage Page (Vendor Defined 0xFF00)
    0x09, 0x01, //       Usage (0x01)
    0xa1, 0x01, //       Collection (Application)
    0x09, 0x02, //         Usage (0x02)
    0x15, 0x00, //         Logical Minimum (0)
    0x26, 0xff, 0x00, //   Logical Maximum (255)
    0x75, 0x08, //         Report Size (8)
    0x95, 0x08, //         Report Count (8)
    0x81, 0x02, //         Input (Data, Var, Abs)
    0x09, 0x03, //         Usage (0x03)
    0x95, 0x08, //         Report Count (8)
    0x91, 0x02, //         Output (Data, Var, Abs)
    0xc0, //             End Collection
];

struct State {
    counter: u32,
    output: [u8; 8],
    next_report: Instant,
}

pub struct HidDevice {
    spec: DeviceSpec,
    state: Mutex<State>,
}

impl HidDevice {
    pub fn new(serial: String) -> Self {
        let len = REPORT_DESCRIPTOR.len() as u16;
        let hid_desc = vec![9, 0x21, 0x11, 0x01, 0, 1, 0x22, len as u8, (len >> 8) as u8];
        HidDevice {
            spec: DeviceSpec {
                vid: MOCK_VID,
                pid: PID_HID,
                bcd_device: 0x0100,
                speed: Speed::Full,
                manufacturer: "usbshare".into(),
                product: "usbshare HID Counter".into(),
                serial,
                interfaces: vec![InterfaceSpec {
                    class: 3,
                    subclass: 0,
                    protocol: 0,
                    extra: hid_desc,
                    endpoints: vec![
                        EndpointSpec {
                            address: 0x81,
                            attributes: 3,
                            max_packet: 8,
                            interval: 10,
                        },
                        EndpointSpec {
                            address: 0x01,
                            attributes: 3,
                            max_packet: 8,
                            interval: 10,
                        },
                    ],
                }],
            },
            state: Mutex::new(State {
                counter: 0,
                output: [0; 8],
                next_report: Instant::now(),
            }),
        }
    }

    fn input_report(st: &State) -> Vec<u8> {
        let mut r = st.counter.to_le_bytes().to_vec();
        r.extend_from_slice(&st.output[..4]);
        r
    }

    fn set_output(&self, data: &[u8]) {
        let mut st = self.state.lock().unwrap();
        st.output = [0; 8];
        let n = data.len().min(8);
        st.output[..n].copy_from_slice(&data[..n]);
    }
}

#[async_trait]
impl MockFunction for HidDevice {
    fn spec(&self) -> &DeviceSpec {
        &self.spec
    }

    async fn control(&self, s: Setup, data: &[u8]) -> Option<UrbResponse> {
        match (s.request_type, s.request) {
            // GET_DESCRIPTOR (interface)
            (0x81, 6) => match (s.value >> 8) as u8 {
                0x22 => Some(UrbResponse::in_data(REPORT_DESCRIPTOR.to_vec())),
                0x21 => Some(UrbResponse::in_data(self.spec.interfaces[0].extra.clone())),
                _ => None,
            },
            // GET_REPORT
            (0xa1, 0x01) => Some(UrbResponse::in_data(Self::input_report(
                &self.state.lock().unwrap(),
            ))),
            // GET_IDLE / GET_PROTOCOL
            (0xa1, 0x02) => Some(UrbResponse::in_data(vec![0])),
            (0xa1, 0x03) => Some(UrbResponse::in_data(vec![1])),
            // SET_REPORT
            (0x21, 0x09) => {
                self.set_output(data);
                Some(UrbResponse::out_ok(data.len()))
            }
            // SET_IDLE / SET_PROTOCOL
            (0x21, 0x0a) | (0x21, 0x0b) => Some(UrbResponse::out_ok(0)),
            _ => None,
        }
    }

    async fn transfer(&self, req: UrbRequest, cancel: &CancellationToken) -> UrbResponse {
        match req.endpoint {
            0x81 => {
                let at = self.state.lock().unwrap().next_report;
                tokio::select! {
                    _ = tokio::time::sleep_until(at) => {}
                    _ = cancel.cancelled() => return UrbResponse::cancelled(),
                }
                let mut st = self.state.lock().unwrap();
                st.counter = st.counter.wrapping_add(1);
                st.next_report = Instant::now() + REPORT_INTERVAL;
                UrbResponse::in_data(Self::input_report(&st))
            }
            0x01 => {
                self.set_output(&req.data);
                UrbResponse::out_ok(req.data.len())
            }
            _ => UrbResponse::stall(),
        }
    }

    fn reset(&self) {
        let mut st = self.state.lock().unwrap();
        st.counter = 0;
        st.output = [0; 8];
        st.next_report = Instant::now();
    }
}
