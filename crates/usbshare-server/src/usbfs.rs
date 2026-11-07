//! Real USB devices on Linux, accessed through usbfs (`/dev/bus/usb`) with `nusb`.
//!
//! Limitations (tracked for later phases):
//! * isochronous endpoints are not supported yet (nusb 0.2 lacks them);
//! * one transfer is in flight per endpoint at a time, which limits bulk throughput.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use nusb::descriptors::TransferType;
use nusb::transfer::{
    Buffer, Bulk, ControlIn, ControlOut, ControlType, In, Interrupt, Out, Recipient, TransferError,
};
use nusb::{DeviceInfo, Endpoint, Interface};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use usbip_proto::{errno, DeviceDescriptor, ExportedDevice, InterfaceDescriptor, Speed};

use crate::device::{req, Backend, Setup, UrbRequest, UrbResponse, UsbDevice};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// Which physical devices may be shared.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Entries are bus ids (`1-1.2`) or `vid:pid` in hex. Empty = all non-hub devices.
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

impl Filter {
    fn matches_entry(entry: &str, busid: &str, vid: u16, pid: u16) -> bool {
        if entry == busid {
            return true;
        }
        if let Some((v, p)) = entry.split_once(':') {
            let v = u16::from_str_radix(v, 16).ok();
            let p = if p == "*" {
                None
            } else {
                u16::from_str_radix(p, 16).ok()
            };
            return v == Some(vid) && p.is_none_or(|p| p == pid);
        }
        false
    }

    pub fn allows(&self, busid: &str, vid: u16, pid: u16) -> bool {
        if self.deny.iter().any(|e| Self::matches_entry(e, busid, vid, pid)) {
            return false;
        }
        self.allow.is_empty() || self.allow.iter().any(|e| Self::matches_entry(e, busid, vid, pid))
    }
}

pub struct UsbfsBackend {
    filter: Filter,
}

impl UsbfsBackend {
    pub fn new(filter: Filter) -> Self {
        UsbfsBackend { filter }
    }

    async fn shareable(&self) -> anyhow::Result<Vec<DeviceInfo>> {
        // No USB subsystem at all (e.g. a container without /sys/bus/usb): nothing to share.
        if !Path::new("/sys/bus/usb/devices").exists() {
            return Ok(Vec::new());
        }
        let list = nusb::list_devices().await.context("enumerating USB devices")?;
        Ok(list
            .filter(|d| d.class() != 0x09) // never share hubs
            .filter(|d| {
                let busid = busid_of(d);
                // Devices attached through vhci (i.e. imported from another server)
                // must not be re-exported.
                !d.sysfs_path()
                    .canonicalize()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .contains("vhci_hcd")
                    && !busid.is_empty()
                    && self.filter.allows(&busid, d.vendor_id(), d.product_id())
            })
            .collect())
    }
}

fn busid_of(d: &DeviceInfo) -> String {
    d.sysfs_path()
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn sysfs_u8(dir: &Path, name: &str) -> Option<u8> {
    std::fs::read_to_string(dir.join(name)).ok()?.trim().parse().ok()
}

fn speed_of(d: &DeviceInfo) -> Speed {
    match d.speed() {
        Some(nusb::Speed::Low) => Speed::Low,
        Some(nusb::Speed::Full) => Speed::Full,
        Some(nusb::Speed::High) => Speed::High,
        Some(nusb::Speed::Super) => Speed::Super,
        Some(nusb::Speed::SuperPlus) => Speed::SuperPlus,
        _ => Speed::Unknown,
    }
}

fn export(d: &DeviceInfo) -> ExportedDevice {
    let path = d.sysfs_path();
    let interfaces: Vec<InterfaceDescriptor> = d
        .interfaces()
        .map(|i| InterfaceDescriptor {
            class: i.class(),
            subclass: i.subclass(),
            protocol: i.protocol(),
        })
        .collect();
    ExportedDevice {
        device: DeviceDescriptor {
            path: path.to_string_lossy().into_owned(),
            busid: busid_of(d),
            busnum: d.busnum() as u32,
            devnum: d.device_address() as u32,
            speed: speed_of(d),
            id_vendor: d.vendor_id(),
            id_product: d.product_id(),
            bcd_device: d.device_version(),
            device_class: d.class(),
            device_subclass: d.subclass(),
            device_protocol: d.protocol(),
            configuration_value: sysfs_u8(path, "bConfigurationValue").unwrap_or(1),
            num_configurations: sysfs_u8(path, "bNumConfigurations").unwrap_or(1),
            num_interfaces: interfaces.len() as u8,
        },
        interfaces,
    }
}

#[async_trait]
impl Backend for UsbfsBackend {
    fn name(&self) -> &'static str {
        "usbfs"
    }

    async fn list(&self) -> anyhow::Result<Vec<ExportedDevice>> {
        Ok(self.shareable().await?.iter().map(export).collect())
    }

    async fn open(&self, busid: &str) -> anyhow::Result<Arc<dyn UsbDevice>> {
        let info = self
            .shareable()
            .await?
            .into_iter()
            .find(|d| busid_of(d) == busid)
            .ok_or_else(|| anyhow!("device {busid} not found"))?;
        let device = info.open().await.with_context(|| format!("opening {busid}"))?;
        let dev = NusbDevice {
            busid: busid.to_string(),
            device,
            interfaces: Mutex::new(HashMap::new()),
            endpoints: Mutex::new(HashMap::new()),
        };
        dev.claim_all().await?;
        info!(%busid, "claimed device (kernel drivers detached)");
        Ok(Arc::new(dev))
    }
}

enum Ep {
    BulkIn(Endpoint<Bulk, In>),
    BulkOut(Endpoint<Bulk, Out>),
    IntIn(Endpoint<Interrupt, In>),
    IntOut(Endpoint<Interrupt, Out>),
}

/// Run one transfer on an endpoint, honouring cancellation.
macro_rules! run_transfer {
    ($ep:expr, $buf:expr, $cancel:expr) => {{
        let ep = $ep;
        // Drain anything left behind by an earlier cancelled future.
        while ep.pending() > 0 {
            ep.cancel_all();
            let _ = ep.next_complete().await;
        }
        ep.submit($buf);
        let done = tokio::select! {
            c = ep.next_complete() => Some(c),
            _ = $cancel.cancelled() => None,
        };
        match done {
            Some(c) => c,
            None => {
                ep.cancel_all();
                let _ = ep.next_complete().await;
                return UrbResponse::cancelled();
            }
        }
    }};
}

struct NusbDevice {
    busid: String,
    device: nusb::Device,
    interfaces: Mutex<HashMap<u8, Interface>>,
    endpoints: Mutex<HashMap<u8, Arc<tokio::sync::Mutex<Ep>>>>,
}

fn errno_of(e: TransferError) -> i32 {
    -match e {
        TransferError::Cancelled => errno::ECONNRESET,
        TransferError::Stall => errno::EPIPE,
        TransferError::Disconnected => errno::ESHUTDOWN,
        TransferError::Fault => errno::EPROTO,
        TransferError::InvalidArgument => errno::EINVAL,
        _ => errno::EIO,
    }
}

impl NusbDevice {
    async fn claim_all(&self) -> anyhow::Result<()> {
        let numbers: Vec<u8> = match self.device.active_configuration() {
            Ok(c) => c.interfaces().map(|i| i.interface_number()).collect(),
            Err(_) => Vec::new(), // unconfigured
        };
        for n in numbers {
            let intf = self
                .device
                .detach_and_claim_interface(n)
                .await
                .with_context(|| format!("claiming interface {n}"))?;
            self.interfaces.lock().unwrap().insert(n, intf);
        }
        Ok(())
    }

    fn drop_endpoints(&self, interface: Option<u8>) {
        let intfs = self.interfaces.lock().unwrap();
        let mut eps = self.endpoints.lock().unwrap();
        eps.retain(|addr, _| match interface {
            None => false,
            Some(n) => intfs
                .get(&n)
                .and_then(|i| i.descriptor())
                .is_none_or(|d| d.endpoints().all(|e| e.address() != *addr)),
        });
    }

    fn endpoint(&self, addr: u8) -> Result<Arc<tokio::sync::Mutex<Ep>>, i32> {
        if let Some(ep) = self.endpoints.lock().unwrap().get(&addr) {
            return Ok(ep.clone());
        }
        let intfs = self.interfaces.lock().unwrap();
        for intf in intfs.values() {
            let Some(desc) = intf.descriptor() else { continue };
            let Some(ed) = desc.endpoints().find(|e| e.address() == addr) else {
                continue;
            };
            let is_in = addr & 0x80 != 0;
            let opened = match (ed.transfer_type(), is_in) {
                (TransferType::Bulk, true) => intf.endpoint::<Bulk, In>(addr).map(Ep::BulkIn),
                (TransferType::Bulk, false) => intf.endpoint::<Bulk, Out>(addr).map(Ep::BulkOut),
                (TransferType::Interrupt, true) => intf.endpoint::<Interrupt, In>(addr).map(Ep::IntIn),
                (TransferType::Interrupt, false) => intf.endpoint::<Interrupt, Out>(addr).map(Ep::IntOut),
                _ => return Err(-errno::EXDEV),
            };
            let ep = Arc::new(tokio::sync::Mutex::new(opened.map_err(|e| {
                warn!(addr = format!("{addr:#04x}"), "open endpoint: {e}");
                -errno::EIO
            })?));
            self.endpoints.lock().unwrap().insert(addr, ep.clone());
            return Ok(ep);
        }
        Err(-errno::EPIPE)
    }

    async fn control(&self, req: &UrbRequest, cancel: &CancellationToken) -> UrbResponse {
        let s = Setup::parse(&req.setup);
        // Requests that change host-side state must go through the usbfs APIs.
        if s.kind() == 0 {
            match (s.recipient(), s.request) {
                (0, req::SET_ADDRESS) => return UrbResponse::out_ok(0),
                (0, req::SET_CONFIGURATION) => {
                    debug!(busid = %self.busid, cfg = s.value, "SET_CONFIGURATION");
                    self.drop_endpoints(None);
                    self.interfaces.lock().unwrap().clear();
                    if let Err(e) = self.device.set_configuration(s.value as u8).await {
                        warn!("set_configuration: {e}");
                        return UrbResponse::stall();
                    }
                    if let Err(e) = self.claim_all().await {
                        warn!("re-claim after SET_CONFIGURATION: {e:#}");
                        return UrbResponse::error(errno::EIO);
                    }
                    return UrbResponse::out_ok(0);
                }
                (1, req::SET_INTERFACE) => {
                    let (n, alt) = (s.index as u8, s.value as u8);
                    debug!(busid = %self.busid, n, alt, "SET_INTERFACE");
                    self.drop_endpoints(Some(n));
                    let intf = self.interfaces.lock().unwrap().get(&n).cloned();
                    let Some(intf) = intf else {
                        return UrbResponse::stall();
                    };
                    return match intf.set_alt_setting(alt).await {
                        Ok(()) => UrbResponse::out_ok(0),
                        Err(e) => {
                            warn!("set_alt_setting: {e}");
                            UrbResponse::stall()
                        }
                    };
                }
                (2, req::CLEAR_FEATURE) if s.value == 0 => {
                    let addr = s.index as u8;
                    let ep = match self.endpoint(addr) {
                        Ok(e) => e,
                        Err(st) => return UrbResponse::error(-st),
                    };
                    let mut ep = ep.lock().await;
                    let r = match &mut *ep {
                        Ep::BulkIn(e) => e.clear_halt().await,
                        Ep::BulkOut(e) => e.clear_halt().await,
                        Ep::IntIn(e) => e.clear_halt().await,
                        Ep::IntOut(e) => e.clear_halt().await,
                    };
                    return match r {
                        Ok(()) => UrbResponse::out_ok(0),
                        Err(_) => UrbResponse::stall(),
                    };
                }
                _ => {}
            }
        }

        let control_type = match s.kind() {
            0 => ControlType::Standard,
            1 => ControlType::Class,
            _ => ControlType::Vendor,
        };
        let recipient = match s.recipient() {
            0 => Recipient::Device,
            1 => Recipient::Interface,
            2 => Recipient::Endpoint,
            _ => Recipient::Other,
        };
        if s.is_in() {
            let fut = self.device.control_in(
                ControlIn {
                    control_type,
                    recipient,
                    request: s.request,
                    value: s.value,
                    index: s.index,
                    length: s.length,
                },
                CONTROL_TIMEOUT,
            );
            tokio::select! {
                r = fut => match r {
                    Ok(data) => UrbResponse::in_data(data),
                    Err(e) => UrbResponse { status: errno_of(e), data: vec![], actual_length: 0 },
                },
                _ = cancel.cancelled() => UrbResponse::cancelled(),
            }
        } else {
            let fut = self.device.control_out(
                ControlOut {
                    control_type,
                    recipient,
                    request: s.request,
                    value: s.value,
                    index: s.index,
                    data: &req.data,
                },
                CONTROL_TIMEOUT,
            );
            tokio::select! {
                r = fut => match r {
                    Ok(()) => UrbResponse::out_ok(req.data.len()),
                    Err(e) => UrbResponse { status: errno_of(e), data: vec![], actual_length: 0 },
                },
                _ = cancel.cancelled() => UrbResponse::cancelled(),
            }
        }
    }
}

#[async_trait]
impl UsbDevice for NusbDevice {
    async fn submit(&self, req: UrbRequest, cancel: &CancellationToken) -> UrbResponse {
        if req.is_control() {
            return self.control(&req, cancel).await;
        }
        if req.iso_packets > 0 {
            return UrbResponse::error(errno::EXDEV);
        }
        let ep = match self.endpoint(req.endpoint) {
            Ok(e) => e,
            Err(st) => return UrbResponse::error(-st),
        };
        let mut guard = ep.lock().await;
        let len = req.length as usize;
        // nusb requires IN lengths to be a non-zero multiple of the max packet size.
        let in_buf = |mps: usize| Buffer::new(len.max(1).div_ceil(mps) * mps);
        let completion = match &mut *guard {
            Ep::BulkIn(e) => {
                let b = in_buf(e.max_packet_size());
                run_transfer!(e, b, cancel)
            }
            Ep::IntIn(e) => {
                let b = in_buf(e.max_packet_size());
                run_transfer!(e, b, cancel)
            }
            Ep::BulkOut(e) => run_transfer!(e, Buffer::from(req.data.clone()), cancel),
            Ep::IntOut(e) => run_transfer!(e, Buffer::from(req.data.clone()), cancel),
        };
        let status = match completion.status {
            Ok(()) => 0,
            Err(e) => errno_of(e),
        };
        if req.is_in() {
            let mut data = completion.buffer.into_vec();
            data.truncate(completion.actual_len.min(len));
            UrbResponse {
                status,
                actual_length: data.len() as u32,
                data,
            }
        } else {
            UrbResponse {
                status,
                data: vec![],
                actual_length: completion.actual_len as u32,
            }
        }
    }

    async fn release(&self) {
        self.drop_endpoints(None);
        let intfs: Vec<(u8, Interface)> = self.interfaces.lock().unwrap().drain().collect();
        for (n, intf) in intfs {
            drop(intf);
            if let Err(e) = self.device.attach_kernel_driver(n) {
                debug!(busid = %self.busid, n, "re-attach kernel driver: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Filter;

    #[test]
    fn filter_rules() {
        let f = Filter::default();
        assert!(f.allows("1-1", 0x1234, 0x5678));
        let f = Filter {
            allow: vec!["1-1.2".into(), "046d:*".into()],
            deny: vec![],
        };
        assert!(f.allows("1-1.2", 1, 2));
        assert!(f.allows("3-4", 0x046d, 0xc52b));
        assert!(!f.allows("3-4", 0x046e, 0xc52b));
        let f = Filter {
            allow: vec![],
            deny: vec!["0bda:8153".into()],
        };
        assert!(!f.allows("2-1", 0x0bda, 0x8153));
        assert!(f.allows("2-1", 0x0bda, 0x8154));
    }
}
