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

use crate::device::{req, Backend, DeviceEntry, Setup, UrbFuture, UrbRequest, UrbResponse, UsbDevice};
use crate::usbfs_raw::{
    compact_iso_in, pack_iso_out, RawTransfer, RawUsbfs, URB_TYPE_BULK, URB_TYPE_INTERRUPT, URB_TYPE_ISO,
};

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

    async fn list(&self) -> anyhow::Result<Vec<DeviceEntry>> {
        Ok(self
            .shareable()
            .await?
            .iter()
            .map(|d| DeviceEntry {
                exported: export(d),
                manufacturer: d.manufacturer_string().map(str::to_string),
                product: d.product_string().map(str::to_string),
            })
            .collect())
    }

    async fn open(&self, busid: &str) -> anyhow::Result<Arc<dyn UsbDevice>> {
        let info = self
            .shareable()
            .await?
            .into_iter()
            .find(|d| busid_of(d) == busid)
            .ok_or_else(|| anyhow!("device {busid} not found"))?;
        debug!(%busid, path = %info.sysfs_path().display(), "opening usbfs device");
        let device = info.open().await.with_context(|| format!("opening {busid}"))?;
        debug!(%busid, "usbfs device opened, claiming interfaces");
        let dev = NusbDevice {
            busid: busid.to_string(),
            busnum: info.busnum(),
            devnum: info.device_address(),
            device,
            interfaces: Mutex::new(HashMap::new()),
            endpoints: Mutex::new(HashMap::new()),
            raw: Mutex::new(None),
            raw_ifaces: Mutex::new(HashMap::new()),
        };
        dev.claim_all().await?;
        // Undo the "stop/eject" the server's own storage stack may have sent the disk
        // when its driver was detached; otherwise the remote host sees no medium.
        let config = dev
            .device
            .active_configuration()
            .ok()
            .map(|c| c.as_bytes().to_vec());
        if let Some(config) = config {
            crate::storage::wake_mass_storage(&dev, busid, &config).await;
        }
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
    busnum: u8,
    devnum: u8,
    device: nusb::Device,
    interfaces: Mutex<HashMap<u8, Interface>>,
    endpoints: Mutex<HashMap<u8, Arc<tokio::sync::Mutex<Ep>>>>,
    /// Second usbfs descriptor for interfaces with isochronous endpoints (see `usbfs_raw`).
    raw: Mutex<Option<Arc<RawUsbfs>>>,
    /// Interfaces owned by `raw`, with their current alternate setting.
    raw_ifaces: Mutex<HashMap<u8, u8>>,
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
    /// Interfaces of the active configuration with an isochronous endpoint in any
    /// alternate setting (audio and video streaming interfaces).
    fn iso_interfaces(&self) -> Vec<u8> {
        let Ok(cfg) = self.device.active_configuration() else {
            return Vec::new();
        };
        let mut v: Vec<u8> = cfg
            .interface_alt_settings()
            .filter(|a| {
                a.endpoints()
                    .any(|e| e.transfer_type() == TransferType::Isochronous)
            })
            .map(|a| a.interface_number())
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    fn raw(&self) -> Option<Arc<RawUsbfs>> {
        self.raw.lock().unwrap().clone()
    }

    /// The usbfs URB type of `addr` if it belongs to a raw-owned interface
    /// (current alternate setting).
    fn raw_endpoint(&self, addr: u8) -> Option<u8> {
        let cfg = self.device.active_configuration().ok()?;
        let ifaces = self.raw_ifaces.lock().unwrap();
        cfg.interface_alt_settings()
            .filter(|a| ifaces.get(&a.interface_number()) == Some(&a.alternate_setting()))
            .flat_map(|a| a.endpoints().collect::<Vec<_>>())
            .find(|e| e.address() == addr)
            .map(|e| match e.transfer_type() {
                TransferType::Isochronous => URB_TYPE_ISO,
                TransferType::Interrupt => URB_TYPE_INTERRUPT,
                _ => URB_TYPE_BULK,
            })
    }

    /// Whether `addr` is an endpoint of a raw-owned interface in any alternate setting.
    fn raw_owns_endpoint(&self, addr: u8) -> bool {
        let Ok(cfg) = self.device.active_configuration() else {
            return false;
        };
        let ifaces = self.raw_ifaces.lock().unwrap();
        cfg.interface_alt_settings()
            .filter(|a| ifaces.contains_key(&a.interface_number()))
            .any(|a| a.endpoints().any(|e| e.address() == addr))
    }

    fn release_raw(&self) {
        let ifaces: Vec<u8> = self.raw_ifaces.lock().unwrap().drain().map(|(n, _)| n).collect();
        if let Some(raw) = self.raw.lock().unwrap().take() {
            for n in ifaces {
                raw.release(n);
            }
            raw.shutdown();
        }
    }

    async fn claim_all(&self) -> anyhow::Result<()> {
        let numbers: Vec<u8> = match self.device.active_configuration() {
            Ok(c) => c.interfaces().map(|i| i.interface_number()).collect(),
            Err(_) => Vec::new(), // unconfigured
        };
        let iso = self.iso_interfaces();
        if !iso.is_empty() && self.raw().is_none() {
            let raw = RawUsbfs::open(self.busnum, self.devnum)
                .context("opening usbfs for isochronous interfaces")?;
            *self.raw.lock().unwrap() = Some(raw);
        }
        for n in numbers {
            if iso.contains(&n) {
                debug!(busid = %self.busid, interface = n, "claiming isochronous interface (raw usbfs)");
                let raw = self.raw().expect("opened above");
                raw.claim(n).with_context(|| format!("claiming interface {n}"))?;
                // Detaching the kernel driver resets the interface to alternate setting 0.
                self.raw_ifaces.lock().unwrap().insert(n, 0);
                continue;
            }
            debug!(busid = %self.busid, interface = n, "detaching kernel driver and claiming interface");
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

    async fn raw_control(
        &self,
        raw: &Arc<RawUsbfs>,
        s: Setup,
        req: &UrbRequest,
        cancel: &CancellationToken,
    ) -> UrbResponse {
        let to_resp = |r: Result<(), i32>| match r {
            Ok(()) => UrbResponse::out_ok(0),
            Err(st) => UrbResponse {
                status: st,
                ..Default::default()
            },
        };
        if s.kind() == 0 && s.recipient() == 1 && s.request == req::SET_INTERFACE {
            let (n, alt) = (s.index as u8, s.value as u8);
            debug!(busid = %self.busid, n, alt, "SET_INTERFACE (raw)");
            let r = raw.set_interface(n, alt).await;
            if r.is_ok() {
                self.raw_ifaces.lock().unwrap().insert(n, alt);
            } else {
                warn!(busid = %self.busid, n, alt, "set interface failed: {r:?}");
            }
            return to_resp(r);
        }
        if s.kind() == 0 && s.recipient() == 2 && s.request == req::CLEAR_FEATURE && s.value == 0 {
            return to_resp(raw.clear_halt(s.index as u8).await);
        }
        let fut = raw.control(req.setup, req.data.clone());
        tokio::select! {
            r = fut => match r {
                Ok(data) if s.is_in() => UrbResponse::in_data(data),
                Ok(_) => UrbResponse::out_ok(req.data.len()),
                Err(st) => UrbResponse { status: st, ..Default::default() },
            },
            _ = cancel.cancelled() => UrbResponse::cancelled(),
        }
    }

    /// Start a transfer on a raw-owned endpoint; it is queued before this returns.
    fn raw_start(
        &self,
        raw: &Arc<RawUsbfs>,
        kind: u8,
        req: UrbRequest,
        cancel: CancellationToken,
    ) -> UrbFuture {
        let is_in = req.is_in();
        let packets = req.iso.clone();
        let buf = match (kind == URB_TYPE_ISO, is_in) {
            (true, true) => vec![0u8; packets.iter().map(|p| p.length as usize).sum()],
            (true, false) => pack_iso_out(&req.data, &packets),
            (false, true) => vec![0u8; req.length as usize],
            (false, false) => req.data,
        };
        let t = RawTransfer {
            kind,
            endpoint: req.endpoint,
            buf,
            iso_lengths: packets.iter().map(|p| p.length).collect(),
            transfer_flags: req.transfer_flags,
        };
        let (key, mut rx) = match raw.submit(t) {
            Ok(v) => v,
            Err(st) => {
                return Box::pin(async move {
                    UrbResponse {
                        status: st,
                        ..Default::default()
                    }
                })
            }
        };
        let raw = raw.clone();
        Box::pin(async move {
            let done = tokio::select! {
                r = &mut rx => Some(r),
                _ = cancel.cancelled() => None,
            };
            let c = match done {
                Some(Ok(c)) => c,
                Some(Err(_)) => return UrbResponse::error(errno::ESHUTDOWN),
                None => {
                    raw.discard(key);
                    let _ = rx.await;
                    return UrbResponse::cancelled();
                }
            };
            if kind == URB_TYPE_ISO {
                let data = if is_in {
                    compact_iso_in(&c.buf, &packets, &c.iso)
                } else {
                    Vec::new()
                };
                let actual = if is_in { data.len() as u32 } else { c.actual_length };
                UrbResponse {
                    status: c.status,
                    data,
                    actual_length: actual,
                    iso: c.iso,
                    start_frame: c.start_frame,
                }
            } else if is_in {
                let mut data = c.buf;
                data.truncate(c.actual_length as usize);
                UrbResponse {
                    status: c.status,
                    actual_length: data.len() as u32,
                    data,
                    ..Default::default()
                }
            } else {
                UrbResponse {
                    status: c.status,
                    actual_length: c.actual_length,
                    ..Default::default()
                }
            }
        })
    }

    async fn control(&self, req: &UrbRequest, cancel: &CancellationToken) -> UrbResponse {
        let s = Setup::parse(&req.setup);
        // Requests addressed to raw-owned interfaces or their endpoints must use the
        // descriptor that claimed them.
        if let Some(raw) = self.raw() {
            let target = (s.index & 0xff) as u8;
            let raw_iface = s.recipient() == 1 && self.raw_ifaces.lock().unwrap().contains_key(&target);
            let raw_ep = s.recipient() == 2 && self.raw_owns_endpoint(target);
            if raw_iface || raw_ep {
                return self.raw_control(&raw, s, req, cancel).await;
            }
        }
        // Requests that change host-side state must go through the usbfs APIs.
        if s.kind() == 0 {
            match (s.recipient(), s.request) {
                (0, req::SET_ADDRESS) => return UrbResponse::out_ok(0),
                (0, req::SET_CONFIGURATION) => {
                    let wanted = s.value as u8;
                    let active = self
                        .device
                        .active_configuration()
                        .ok()
                        .map(|c| c.configuration_value());
                    debug!(busid = %self.busid, wanted, ?active, "SET_CONFIGURATION");
                    // The remote host always sets the configuration while enumerating. If it is
                    // already active, leave the device alone (as the kernel's usbip-host does):
                    // re-setting it fails with EBUSY while interfaces are held, and would
                    // needlessly reset the device.
                    if active == Some(wanted) {
                        return UrbResponse::out_ok(0);
                    }
                    self.drop_endpoints(None);
                    self.interfaces.lock().unwrap().clear();
                    self.release_raw();
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
                (0, req::GET_DESCRIPTOR) if s.is_in() && (s.value >> 8) == 2 => {
                    // Serve configuration descriptors with UAS alternate settings removed.
                    let index = (s.value & 0xff) as usize;
                    if let Some(cfg) = self.device.configurations().nth(index) {
                        let mut d = strip_uas(cfg.as_bytes());
                        d.truncate(s.length as usize);
                        return UrbResponse::in_data(d);
                    }
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
                    Err(e) => UrbResponse { status: errno_of(e), ..Default::default() },
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
                    Err(e) => UrbResponse { status: errno_of(e), ..Default::default() },
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
        if let (Some(raw), Some(kind)) = (self.raw(), self.raw_endpoint(req.endpoint)) {
            return self.raw_start(&raw, kind, req, cancel.clone()).await;
        }
        if req.is_iso() {
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
                ..Default::default()
            }
        } else {
            UrbResponse {
                status,
                data: vec![],
                actual_length: completion.actual_len as u32,
                ..Default::default()
            }
        }
    }

    fn enqueue(&self, req: UrbRequest, cancel: CancellationToken) -> Result<UrbFuture, UrbRequest> {
        if req.is_control() {
            return Err(req);
        }
        match (self.raw(), self.raw_endpoint(req.endpoint)) {
            (Some(raw), Some(kind)) => Ok(self.raw_start(&raw, kind, req, cancel)),
            _ => Err(req),
        }
    }

    async fn release(&self) {
        self.release_raw();
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

/// Remove USB Attached SCSI (UAS) alternate settings from a configuration descriptor.
///
/// UAS needs USB 3 bulk streams, which the USB/IP protocol cannot carry, so a remote
/// host that picks UAS hangs. Mass-storage devices that support UAS also offer the
/// classic Bulk-Only Transport as another alternate setting of the same interface;
/// hiding the UAS one makes the remote host use `usb-storage` instead. An interface
/// whose only alternate setting is UAS is left unchanged.
pub fn strip_uas(config: &[u8]) -> Vec<u8> {
    const UAS: (u8, u8) = (0x08, 0x62); // class mass storage, protocol UAS
                                        // Split into descriptors.
    let mut descs: Vec<&[u8]> = Vec::new();
    let mut off = 0;
    while off + 2 <= config.len() {
        let len = config[off] as usize;
        if len < 2 || off + len > config.len() {
            break;
        }
        descs.push(&config[off..off + len]);
        off += len;
    }
    if descs.first().is_none_or(|d| d[1] != 2 || d.len() < 9) {
        return config.to_vec();
    }
    let is_intf = |d: &[u8]| d[1] == 4 && d.len() >= 9;
    let is_uas = |d: &[u8]| is_intf(d) && (d[5], d[7]) == UAS;
    // Interfaces that also have a non-UAS alternate setting.
    let has_fallback: Vec<u8> = descs
        .iter()
        .filter(|d| is_intf(d) && !is_uas(d))
        .map(|d| d[2])
        .collect();

    let mut out = descs[0].to_vec();
    let mut skipping = false;
    for d in &descs[1..] {
        if is_intf(d) || d[1] == 11 {
            // A new interface (or interface association) ends the previous one.
            skipping = is_uas(d) && has_fallback.contains(&d[2]);
        }
        if !skipping {
            out.extend_from_slice(d);
        }
    }
    let total = out.len() as u16;
    out[2..4].copy_from_slice(&total.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::{strip_uas, Filter};

    fn intf(num: u8, alt: u8, protocol: u8, eps: u8) -> Vec<u8> {
        vec![9, 4, num, alt, eps, 0x08, 0x06, protocol, 0]
    }
    fn ep(addr: u8) -> Vec<u8> {
        vec![7, 5, addr, 2, 0x00, 0x04, 0]
    }
    fn config(body: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = body.concat();
        let total = (9 + body.len()) as u16;
        let mut c = vec![9, 2, total as u8, (total >> 8) as u8, 1, 1, 0, 0x80, 50];
        c.extend(body);
        c
    }

    #[test]
    fn strips_uas_alt_setting() {
        // Typical UAS stick: alt 0 = Bulk-Only (0x50), alt 1 = UAS (0x62) with 4 endpoints
        // plus SuperSpeed companion and pipe usage descriptors.
        let ss_companion = vec![6, 0x30, 0, 0, 0, 0];
        let pipe_usage = vec![4, 0x24, 1, 0];
        let bot = [
            intf(0, 0, 0x50, 2),
            ep(0x81),
            ss_companion.clone(),
            ep(0x02),
            ss_companion.clone(),
        ];
        let uas = [
            intf(0, 1, 0x62, 4),
            ep(0x81),
            ss_companion.clone(),
            pipe_usage.clone(),
            ep(0x02),
            ss_companion.clone(),
            pipe_usage.clone(),
            ep(0x83),
            ss_companion.clone(),
            pipe_usage.clone(),
            ep(0x04),
            ss_companion.clone(),
            pipe_usage,
        ];
        let full = config(&[bot.to_vec(), uas.to_vec()].concat());
        let stripped = strip_uas(&full);
        assert_eq!(stripped, config(&bot));
        assert_eq!(
            u16::from_le_bytes([stripped[2], stripped[3]]) as usize,
            stripped.len()
        );
    }

    #[test]
    fn keeps_uas_only_interface_and_non_storage_configs() {
        let only_uas = config(&[intf(0, 0, 0x62, 1), ep(0x81)]);
        assert_eq!(strip_uas(&only_uas), only_uas);
        let bot_only = config(&[intf(0, 0, 0x50, 2), ep(0x81), ep(0x02)]);
        assert_eq!(strip_uas(&bot_only), bot_only);
        assert_eq!(strip_uas(&[1, 2, 3]), vec![1, 2, 3]);
    }

    /// nusb runs blocking syscalls (open, claim, set configuration) on a thread pool,
    /// which needs its `tokio` feature. Without it, awaiting them panics. This goes
    /// through that path without needing USB hardware: opening /dev/null as a device
    /// must fail with an error, not a panic.
    #[tokio::test]
    async fn nusb_blocking_calls_work_under_tokio() {
        let fd: std::os::fd::OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        assert!(nusb::Device::from_fd(fd).await.is_err());
    }

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
