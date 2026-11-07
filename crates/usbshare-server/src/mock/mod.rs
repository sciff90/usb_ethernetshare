//! Software-simulated USB devices.
//!
//! These let the whole stack be exercised without any USB hardware or kernel
//! support on the server: a remote Linux host attaching them through vhci-hcd
//! sees real USB devices (a vendor-specific loopback device, a HID device and
//! a USB mass-storage disk), and the bundled `usbshare test` command can drive
//! them purely in user space.

mod hid;
mod loopback;
mod storage;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use usbip_proto::{DeviceDescriptor, ExportedDevice, InterfaceDescriptor, Speed};

use crate::device::{req, Backend, DeviceEntry, Setup, UrbRequest, UrbResponse, UsbDevice};

pub use hid::HidDevice;
pub use loopback::LoopbackDevice;
pub use storage::StorageDevice;

/// Vendor ID from pid.codes (open-source hardware), with its test PIDs.
pub const MOCK_VID: u16 = 0x1209;
pub const PID_LOOPBACK: u16 = 0x0001;
pub const PID_HID: u16 = 0x0002;
pub const PID_STORAGE: u16 = 0x0003;

/// Mock devices live on a fake bus number so they never clash with real ones.
pub const MOCK_BUSNUM: u32 = 100;

#[derive(Debug, Clone)]
pub struct EndpointSpec {
    pub address: u8,
    /// 2 = bulk, 3 = interrupt.
    pub attributes: u8,
    pub max_packet: u16,
    pub interval: u8,
}

#[derive(Debug, Clone)]
pub struct InterfaceSpec {
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    /// Class-specific descriptors placed between the interface and endpoint descriptors.
    pub extra: Vec<u8>,
    pub endpoints: Vec<EndpointSpec>,
}

#[derive(Debug, Clone)]
pub struct DeviceSpec {
    pub vid: u16,
    pub pid: u16,
    pub bcd_device: u16,
    pub speed: Speed,
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
    pub interfaces: Vec<InterfaceSpec>,
}

impl DeviceSpec {
    pub fn device_descriptor(&self) -> Vec<u8> {
        let bcd_usb: u16 = if self.speed == Speed::High { 0x0200 } else { 0x0110 };
        let mut d = vec![18, 1];
        d.extend_from_slice(&bcd_usb.to_le_bytes());
        d.extend_from_slice(&[0, 0, 0, 64]); // class per interface, ep0 max packet 64
        d.extend_from_slice(&self.vid.to_le_bytes());
        d.extend_from_slice(&self.pid.to_le_bytes());
        d.extend_from_slice(&self.bcd_device.to_le_bytes());
        d.extend_from_slice(&[1, 2, 3, 1]); // string indices, 1 configuration
        d
    }

    pub fn config_descriptor(&self) -> Vec<u8> {
        let mut body = Vec::new();
        for (n, i) in self.interfaces.iter().enumerate() {
            body.extend_from_slice(&[
                9,
                4,
                n as u8,
                0,
                i.endpoints.len() as u8,
                i.class,
                i.subclass,
                i.protocol,
                0,
            ]);
            body.extend_from_slice(&i.extra);
            for e in &i.endpoints {
                body.extend_from_slice(&[7, 5, e.address, e.attributes]);
                body.extend_from_slice(&e.max_packet.to_le_bytes());
                body.push(e.interval);
            }
        }
        let total = (9 + body.len()) as u16;
        let mut d = vec![9, 2];
        d.extend_from_slice(&total.to_le_bytes());
        d.extend_from_slice(&[self.interfaces.len() as u8, 1, 0, 0x80, 50]);
        d.extend_from_slice(&body);
        d
    }

    pub fn string_descriptor(&self, index: u8) -> Option<Vec<u8>> {
        let s = match index {
            0 => return Some(vec![4, 3, 0x09, 0x04]), // en-US
            1 => &self.manufacturer,
            2 => &self.product,
            3 => &self.serial,
            _ => return None,
        };
        let mut d = vec![0, 3];
        for u in s.encode_utf16() {
            d.extend_from_slice(&u.to_le_bytes());
        }
        d[0] = d.len() as u8;
        Some(d)
    }

    pub fn exported(&self, busid: &str, devnum: u32) -> ExportedDevice {
        ExportedDevice {
            device: DeviceDescriptor {
                path: format!("/usbshare/mock/{busid}"),
                busid: busid.to_string(),
                busnum: MOCK_BUSNUM,
                devnum,
                speed: self.speed,
                id_vendor: self.vid,
                id_product: self.pid,
                bcd_device: self.bcd_device,
                device_class: 0,
                device_subclass: 0,
                device_protocol: 0,
                configuration_value: 1,
                num_configurations: 1,
                num_interfaces: self.interfaces.len() as u8,
            },
            interfaces: self
                .interfaces
                .iter()
                .map(|i| InterfaceDescriptor {
                    class: i.class,
                    subclass: i.subclass,
                    protocol: i.protocol,
                })
                .collect(),
        }
    }
}

/// The device-specific part of a mock device. Standard requests are
/// handled by [`MockDevice`].
#[async_trait]
pub trait MockFunction: Send + Sync {
    fn spec(&self) -> &DeviceSpec;

    /// Class/vendor control requests and interface-level GET_DESCRIPTOR.
    /// `None` stalls the request.
    async fn control(&self, _setup: Setup, _data: &[u8]) -> Option<UrbResponse> {
        None
    }

    /// Bulk / interrupt transfers on non-zero endpoints.
    async fn transfer(&self, req: UrbRequest, cancel: &CancellationToken) -> UrbResponse;

    /// Reset volatile state (called when a host attaches or configures the device).
    fn reset(&self) {}
}

/// Wraps a [`MockFunction`] and implements the USB chapter 9 standard requests.
pub struct MockDevice {
    function: Box<dyn MockFunction>,
    configuration: Mutex<u8>,
}

impl MockDevice {
    pub fn new(function: impl MockFunction + 'static) -> Self {
        MockDevice {
            function: Box::new(function),
            configuration: Mutex::new(0),
        }
    }

    pub fn spec(&self) -> &DeviceSpec {
        self.function.spec()
    }

    fn standard(&self, s: Setup) -> Option<UrbResponse> {
        let spec = self.function.spec();
        let reply = |v: Vec<u8>| Some(UrbResponse::in_data(v));
        match (s.recipient(), s.request) {
            (0, req::GET_DESCRIPTOR) => {
                let (ty, idx) = ((s.value >> 8) as u8, s.value as u8);
                match ty {
                    1 => reply(spec.device_descriptor()),
                    2 if idx == 0 => reply(spec.config_descriptor()),
                    3 => spec.string_descriptor(idx).map(UrbResponse::in_data),
                    _ => None, // e.g. device qualifier / BOS: stall
                }
            }
            (0, req::SET_ADDRESS) => Some(UrbResponse::out_ok(0)),
            (0, req::GET_CONFIGURATION) => reply(vec![*self.configuration.lock().unwrap()]),
            (0, req::SET_CONFIGURATION) => {
                let v = s.value as u8;
                if v > 1 {
                    return None;
                }
                *self.configuration.lock().unwrap() = v;
                self.function.reset();
                Some(UrbResponse::out_ok(0))
            }
            (_, req::GET_STATUS) => reply(vec![0, 0]),
            (_, req::CLEAR_FEATURE) | (_, req::SET_FEATURE) => Some(UrbResponse::out_ok(0)),
            (1, req::GET_INTERFACE) => reply(vec![0]),
            (1, req::SET_INTERFACE) => (s.value == 0).then(|| UrbResponse::out_ok(0)),
            _ => None,
        }
    }
}

#[async_trait]
impl UsbDevice for MockDevice {
    async fn submit(&self, req: UrbRequest, cancel: &CancellationToken) -> UrbResponse {
        if req.iso_packets > 0 {
            return UrbResponse::error(usbip_proto::errno::EXDEV);
        }
        if !req.is_control() {
            return self.function.transfer(req, cancel).await;
        }
        let setup = Setup::parse(&req.setup);
        // Interface-recipient GET_DESCRIPTOR (e.g. HID report descriptor) goes to the function.
        let resp = if setup.kind() == 0 && !(setup.recipient() == 1 && setup.request == req::GET_DESCRIPTOR) {
            self.standard(setup)
        } else {
            self.function.control(setup, &req.data).await
        };
        match resp {
            Some(mut r) => {
                if setup.is_in() {
                    r.data.truncate(setup.length as usize);
                    r.actual_length = r.data.len() as u32;
                } else if r.status == 0 {
                    r.actual_length = req.data.len() as u32;
                }
                r
            }
            None => UrbResponse::stall(),
        }
    }
}

/// Which mock device to create, parsed from the command line
/// (`loopback`, `hid`, `storage`, `storage:64` for a 64 MiB disk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockKind {
    Loopback,
    Hid,
    Storage { size_mib: u32 },
}

impl std::str::FromStr for MockKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (name, arg) = match s.split_once(':') {
            Some((n, a)) => (n, Some(a)),
            None => (s, None),
        };
        match (name, arg) {
            ("loopback", None) => Ok(MockKind::Loopback),
            ("hid", None) => Ok(MockKind::Hid),
            ("storage", None) => Ok(MockKind::Storage { size_mib: 16 }),
            ("storage", Some(a)) => {
                let size_mib: u32 = a
                    .trim_end_matches(['M', 'm'])
                    .parse()
                    .map_err(|_| format!("bad size '{a}'"))?;
                if !(1..=4096).contains(&size_mib) {
                    return Err("storage size must be 1..=4096 MiB".into());
                }
                Ok(MockKind::Storage { size_mib })
            }
            _ => Err(format!(
                "unknown mock device '{s}' (expected loopback, hid, storage[:MiB])"
            )),
        }
    }
}

/// A backend serving a fixed set of mock devices with bus ids `100-1`, `100-2`, ...
pub struct MockBackend {
    devices: Vec<(String, Arc<MockDevice>)>,
}

impl MockBackend {
    pub fn new(kinds: &[MockKind]) -> Self {
        let devices = kinds
            .iter()
            .enumerate()
            .map(|(i, k)| {
                let serial = format!("USBSHARE{:04}", i + 1);
                let dev = match k {
                    MockKind::Loopback => MockDevice::new(LoopbackDevice::new(serial)),
                    MockKind::Hid => MockDevice::new(HidDevice::new(serial)),
                    MockKind::Storage { size_mib } => MockDevice::new(StorageDevice::new(serial, *size_mib)),
                };
                (format!("{MOCK_BUSNUM}-{}", i + 1), Arc::new(dev))
            })
            .collect();
        MockBackend { devices }
    }
}

#[async_trait]
impl Backend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn list(&self) -> anyhow::Result<Vec<DeviceEntry>> {
        Ok(self
            .devices
            .iter()
            .enumerate()
            .map(|(i, (busid, d))| DeviceEntry {
                exported: d.spec().exported(busid, i as u32 + 1),
                manufacturer: Some(d.spec().manufacturer.clone()),
                product: Some(d.spec().product.clone()),
            })
            .collect())
    }

    async fn open(&self, busid: &str) -> anyhow::Result<Arc<dyn UsbDevice>> {
        let (_, dev) = self
            .devices
            .iter()
            .find(|(b, _)| b == busid)
            .ok_or_else(|| anyhow::anyhow!("no such mock device {busid}"))?;
        *dev.configuration.lock().unwrap() = 0;
        dev.function.reset();
        Ok(dev.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kinds() {
        assert_eq!("loopback".parse::<MockKind>().unwrap(), MockKind::Loopback);
        assert_eq!(
            "storage:64M".parse::<MockKind>().unwrap(),
            MockKind::Storage { size_mib: 64 }
        );
        assert!("storage:0".parse::<MockKind>().is_err());
        assert!("webcam".parse::<MockKind>().is_err());
    }

    #[test]
    fn descriptors_are_well_formed() {
        for k in [
            MockKind::Loopback,
            MockKind::Hid,
            MockKind::Storage { size_mib: 1 },
        ] {
            let b = MockBackend::new(&[k]);
            let spec = b.devices[0].1.spec();
            let dd = spec.device_descriptor();
            assert_eq!(dd.len(), 18);
            let cd = spec.config_descriptor();
            assert_eq!(u16::from_le_bytes([cd[2], cd[3]]) as usize, cd.len());
            // Walk the descriptor chain: lengths must tile the buffer exactly.
            let mut off = 0;
            while off < cd.len() {
                assert!(cd[off] >= 2);
                off += cd[off] as usize;
            }
            assert_eq!(off, cd.len());
            let s = spec.string_descriptor(2).unwrap();
            assert_eq!(s[0] as usize, s.len());
        }
    }
}
