//! Direct use of the Linux usbfs ioctl interface (`/dev/bus/usb/BBB/DDD`).
//!
//! `nusb` does not support isochronous transfers, which webcams and audio
//! devices stream over. Interfaces that have isochronous endpoints are therefore
//! claimed through a second file descriptor on the same device and driven here.
//! Transfers are submitted asynchronously (`USBDEVFS_SUBMITURB`) and reaped
//! when the descriptor polls writable, so any number can be in flight.
//!
//! Each usbfs file descriptor only sees the URBs it submitted, so this does not
//! interfere with nusb's own descriptor for the other interfaces.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::device::{IsoRequest, IsoResult};

// ---------------------------------------------------------------------------
// Kernel ABI (include/uapi/linux/usbdevice_fs.h)

pub const URB_TYPE_ISO: u8 = 0;
pub const URB_TYPE_INTERRUPT: u8 = 1;
pub const URB_TYPE_BULK: u8 = 3;

const URB_SHORT_NOT_OK: u32 = 0x01;
const URB_ISO_ASAP: u32 = 0x02;
const URB_ZERO_PACKET: u32 = 0x40;

/// Maximum isochronous packets per usbfs URB.
pub const MAX_ISO_PACKETS: usize = 128;

#[repr(C)]
struct UsbdevfsUrb {
    kind: u8,
    endpoint: u8,
    status: i32,
    flags: u32,
    buffer: *mut libc::c_void,
    buffer_length: i32,
    actual_length: i32,
    start_frame: i32,
    number_of_packets: i32,
    error_count: i32,
    signr: u32,
    usercontext: *mut libc::c_void,
    // followed by `number_of_packets` x IsoPacketDesc
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IsoPacketDesc {
    length: u32,
    actual_length: u32,
    status: u32,
}

#[repr(C)]
struct CtrlTransfer {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    timeout: u32,
    data: *mut libc::c_void,
}

#[repr(C)]
struct SetInterface {
    interface: u32,
    altsetting: u32,
}

#[repr(C)]
struct DisconnectClaim {
    interface: u32,
    flags: u32,
    driver: [u8; 256],
}

#[repr(C)]
struct UsbdevfsIoctl {
    ifno: i32,
    ioctl_code: i32,
    data: *mut libc::c_void,
}

const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((b'U' as u64) << 8) | nr
}
const IOC_NONE: u64 = 0;
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;

const USBDEVFS_CONTROL: u64 = ioc(IOC_READ | IOC_WRITE, 0, std::mem::size_of::<CtrlTransfer>());
const USBDEVFS_SETINTERFACE: u64 = ioc(IOC_READ, 4, std::mem::size_of::<SetInterface>());
const USBDEVFS_SUBMITURB: u64 = ioc(IOC_READ, 10, std::mem::size_of::<UsbdevfsUrb>());
const USBDEVFS_DISCARDURB: u64 = ioc(IOC_NONE, 11, 0);
const USBDEVFS_REAPURBNDELAY: u64 = ioc(IOC_WRITE, 13, std::mem::size_of::<*mut libc::c_void>());
const USBDEVFS_RELEASEINTERFACE: u64 = ioc(IOC_READ, 16, std::mem::size_of::<u32>());
const USBDEVFS_IOCTL: u64 = ioc(IOC_READ | IOC_WRITE, 18, std::mem::size_of::<UsbdevfsIoctl>());
const USBDEVFS_CLEAR_HALT: u64 = ioc(IOC_READ, 21, std::mem::size_of::<u32>());
const USBDEVFS_CONNECT: u64 = ioc(IOC_NONE, 23, 0);
const USBDEVFS_DISCONNECT_CLAIM: u64 = ioc(IOC_READ, 27, std::mem::size_of::<DisconnectClaim>());

unsafe fn ioctl(fd: RawFd, req: u64, arg: *mut libc::c_void) -> io::Result<libc::c_int> {
    // The request parameter type differs between libc implementations.
    let r = libc::ioctl(fd, req as _, arg);
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn errno_of(e: &io::Error) -> i32 {
    -e.raw_os_error().unwrap_or(libc::EIO)
}

// ---------------------------------------------------------------------------

/// URB memory: the header followed by the iso descriptors, 8-byte aligned. The
/// kernel keeps a pointer to it until the URB is reaped, so it must not move.
struct UrbMem {
    words: Vec<u64>,
    n_iso: usize,
}

impl UrbMem {
    fn new(n_iso: usize) -> Self {
        let bytes = std::mem::size_of::<UsbdevfsUrb>() + n_iso * std::mem::size_of::<IsoPacketDesc>();
        UrbMem {
            words: vec![0u64; bytes.div_ceil(8)],
            n_iso,
        }
    }
    fn header(&mut self) -> *mut UsbdevfsUrb {
        self.words.as_mut_ptr() as *mut UsbdevfsUrb
    }
    fn iso(&mut self) -> &mut [IsoPacketDesc] {
        // SAFETY: the allocation is large enough for the header plus n_iso descriptors,
        // and IsoPacketDesc (align 4) fits the 8-byte aligned offset after the header.
        unsafe {
            let p = (self.words.as_mut_ptr() as *mut u8).add(std::mem::size_of::<UsbdevfsUrb>())
                as *mut IsoPacketDesc;
            std::slice::from_raw_parts_mut(p, self.n_iso)
        }
    }
}

// The raw pointers inside point into memory owned by the same struct.
unsafe impl Send for UrbMem {}

struct Inflight {
    mem: UrbMem,
    buf: Vec<u8>,
    tx: oneshot::Sender<RawCompletion>,
}

/// A finished raw transfer.
#[derive(Debug)]
pub struct RawCompletion {
    /// 0 or negative errno.
    pub status: i32,
    pub actual_length: u32,
    pub start_frame: i32,
    /// The transfer buffer. For isochronous IN transfers, packet `i` starts at the sum
    /// of the requested lengths of packets `0..i`.
    pub buf: Vec<u8>,
    pub iso: Vec<IsoResult>,
}

/// A transfer to start on a raw interface.
pub struct RawTransfer {
    pub kind: u8,
    pub endpoint: u8,
    /// OUT data, or a zeroed buffer of the requested size for IN.
    pub buf: Vec<u8>,
    /// Requested length of each isochronous packet.
    pub iso_lengths: Vec<u32>,
    /// Linux URB transfer flags from the remote host.
    pub transfer_flags: u32,
}

/// A second usbfs file descriptor on a device, owning some of its interfaces.
pub struct RawUsbfs {
    // Field order matters: the descriptor must be closed (cancelling the kernel's
    // URBs) before the URB memory in `inflight` is freed.
    fd: AsyncFd<OwnedFd>,
    inflight: Mutex<HashMap<usize, Inflight>>,
    stop: CancellationToken,
}

impl Drop for RawUsbfs {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl RawUsbfs {
    /// Open `/dev/bus/usb/<bus>/<dev>` and start the completion reaper.
    pub fn open(busnum: u8, devnum: u8) -> io::Result<Arc<RawUsbfs>> {
        let path = format!("/dev/bus/usb/{busnum:03}/{devnum:03}");
        let c = std::ffi::CString::new(path.clone()).unwrap();
        // SAFETY: plain open(2) of a NUL-terminated path.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if fd < 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(e.kind(), format!("{path}: {e}")));
        }
        // SAFETY: fd was just returned by open(2) and is owned by nobody else.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: the descriptor is owned by the AsyncFd and only closed when it is dropped.
        let fd = unsafe { AsyncFd::register(fd)? };
        let raw = Arc::new(RawUsbfs {
            fd,
            inflight: Mutex::new(HashMap::new()),
            stop: CancellationToken::new(),
        });
        let weak = Arc::downgrade(&raw);
        let stop = raw.stop.clone();
        tokio::spawn(async move { reaper(weak, stop).await });
        Ok(raw)
    }

    /// Stop the reaper; in-flight transfers are cancelled when the descriptor closes.
    pub fn shutdown(&self) {
        self.stop.cancel();
    }

    fn raw_fd(&self) -> RawFd {
        self.fd.get_ref().as_raw_fd()
    }

    /// Detach any kernel driver from `interface` and claim it.
    pub fn claim(&self, interface: u8) -> io::Result<()> {
        let mut dc = DisconnectClaim {
            interface: interface as u32,
            flags: 0,
            driver: [0; 256],
        };
        // SAFETY: valid pointer to a correctly laid out struct for this ioctl.
        unsafe {
            ioctl(
                self.raw_fd(),
                USBDEVFS_DISCONNECT_CLAIM,
                &mut dc as *mut _ as *mut _,
            )
        }?;
        Ok(())
    }

    /// Release `interface` and let the kernel re-bind its driver.
    pub fn release(&self, interface: u8) {
        let mut n = interface as u32;
        // SAFETY: valid pointers to correctly laid out arguments.
        unsafe {
            if let Err(e) = ioctl(
                self.raw_fd(),
                USBDEVFS_RELEASEINTERFACE,
                &mut n as *mut _ as *mut _,
            ) {
                debug!(interface, "release interface: {e}");
            }
            let mut cmd = UsbdevfsIoctl {
                ifno: interface as i32,
                ioctl_code: USBDEVFS_CONNECT as i32,
                data: std::ptr::null_mut(),
            };
            if let Err(e) = ioctl(self.raw_fd(), USBDEVFS_IOCTL, &mut cmd as *mut _ as *mut _) {
                debug!(interface, "reconnect kernel driver: {e}");
            }
        }
    }

    pub async fn set_interface(self: &Arc<Self>, interface: u8, alt: u8) -> Result<(), i32> {
        let this = self.clone();
        blocking(move || {
            let mut si = SetInterface {
                interface: interface as u32,
                altsetting: alt as u32,
            };
            // SAFETY: valid pointer to a correctly laid out struct.
            unsafe { ioctl(this.raw_fd(), USBDEVFS_SETINTERFACE, &mut si as *mut _ as *mut _) }.map(|_| ())
        })
        .await
    }

    pub async fn clear_halt(self: &Arc<Self>, endpoint: u8) -> Result<(), i32> {
        let this = self.clone();
        blocking(move || {
            let mut ep = endpoint as u32;
            // SAFETY: valid pointer to a u32.
            unsafe { ioctl(this.raw_fd(), USBDEVFS_CLEAR_HALT, &mut ep as *mut _ as *mut _) }.map(|_| ())
        })
        .await
    }

    /// A control transfer through this descriptor (needed for requests addressed to
    /// interfaces or endpoints that this descriptor has claimed). Returns IN data.
    pub async fn control(self: &Arc<Self>, setup: [u8; 8], out: Vec<u8>) -> Result<Vec<u8>, i32> {
        let this = self.clone();
        blocking(move || {
            let length = u16::from_le_bytes([setup[6], setup[7]]);
            let is_in = setup[0] & 0x80 != 0;
            let mut buf = if is_in { vec![0u8; length as usize] } else { out };
            let mut ct = CtrlTransfer {
                request_type: setup[0],
                request: setup[1],
                value: u16::from_le_bytes([setup[2], setup[3]]),
                index: u16::from_le_bytes([setup[4], setup[5]]),
                length: if is_in { length } else { buf.len() as u16 },
                timeout: 5000,
                data: buf.as_mut_ptr() as *mut _,
            };
            // SAFETY: `buf` is at least `ct.length` bytes and outlives the call.
            let n = unsafe { ioctl(this.raw_fd(), USBDEVFS_CONTROL, &mut ct as *mut _ as *mut _) }?;
            if is_in {
                buf.truncate(n as usize);
                Ok(buf)
            } else {
                Ok(Vec::new())
            }
        })
        .await
    }

    /// Start a transfer. It is queued on the device before this returns.
    pub fn submit(&self, t: RawTransfer) -> Result<(usize, oneshot::Receiver<RawCompletion>), i32> {
        let n_iso = t.iso_lengths.len();
        if n_iso > MAX_ISO_PACKETS {
            return Err(-libc::EINVAL);
        }
        let mut mem = UrbMem::new(n_iso);
        for (d, &len) in mem.iso().iter_mut().zip(&t.iso_lengths) {
            d.length = len;
        }
        let mut buf = t.buf;
        let mut flags = t.transfer_flags & (URB_SHORT_NOT_OK | URB_ZERO_PACKET);
        if t.kind == URB_TYPE_ISO {
            flags = URB_ISO_ASAP;
        }
        let key = mem.header() as usize;
        // SAFETY: header() points to zeroed memory sized for the struct.
        unsafe {
            let h = &mut *mem.header();
            h.kind = t.kind;
            h.endpoint = t.endpoint;
            h.flags = flags;
            h.buffer = buf.as_mut_ptr() as *mut _;
            h.buffer_length = buf.len() as i32;
            h.number_of_packets = n_iso as i32;
            h.usercontext = key as *mut _;
        }
        let (tx, rx) = oneshot::channel();
        // Insert before submitting: the reaper may see the completion immediately.
        let mut inflight = self.inflight.lock().unwrap();
        inflight.insert(key, Inflight { mem, buf, tx });
        // SAFETY: the URB memory and buffer live in `inflight` until reaped (or until
        // the descriptor is closed), and neither moves: they are heap allocations.
        if let Err(e) = unsafe { ioctl(self.raw_fd(), USBDEVFS_SUBMITURB, key as *mut _) } {
            inflight.remove(&key);
            return Err(errno_of(&e));
        }
        Ok((key, rx))
    }

    /// Cancel an in-flight transfer. Its completion is still delivered (with an error).
    pub fn discard(&self, key: usize) {
        // Holding the lock guarantees the URB has not been reaped and freed.
        let inflight = self.inflight.lock().unwrap();
        if inflight.contains_key(&key) {
            // SAFETY: `key` is the address of a URB that is still in flight.
            let _ = unsafe { ioctl(self.raw_fd(), USBDEVFS_DISCARDURB, key as *mut _) };
        }
    }

    /// Reap every completed URB. Returns Err on a fatal error (device gone).
    fn reap_all(&self) -> io::Result<()> {
        loop {
            let mut ptr: *mut libc::c_void = std::ptr::null_mut();
            // SAFETY: the kernel writes back the pointer of a URB we submitted (and the
            // URB's status fields into the memory it points to, which is still alive).
            match unsafe {
                ioctl(
                    self.raw_fd(),
                    USBDEVFS_REAPURBNDELAY,
                    &mut ptr as *mut _ as *mut _,
                )
            } {
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => return Ok(()),
                Err(e) => return Err(e),
            }
            let Some(mut f) = self.inflight.lock().unwrap().remove(&(ptr as usize)) else {
                warn!("usbfs: reaped unknown URB");
                continue;
            };
            // SAFETY: the kernel has finished with this URB.
            let (status, actual, start_frame) = unsafe {
                let h = &*f.mem.header();
                (h.status, h.actual_length, h.start_frame)
            };
            let iso = f
                .mem
                .iso()
                .iter()
                .map(|d| IsoResult {
                    actual_length: d.actual_length,
                    status: d.status as i32,
                })
                .collect();
            let _ = f.tx.send(RawCompletion {
                status,
                actual_length: actual.max(0) as u32,
                start_frame,
                buf: std::mem::take(&mut f.buf),
                iso,
            });
        }
    }

    fn fail_all(&self, status: i32) {
        for (_, f) in self.inflight.lock().unwrap().drain() {
            let _ = f.tx.send(RawCompletion {
                status,
                actual_length: 0,
                start_frame: 0,
                buf: Vec::new(),
                iso: Vec::new(),
            });
        }
    }
}

async fn reaper(raw: std::sync::Weak<RawUsbfs>, stop: CancellationToken) {
    loop {
        let Some(this) = raw.upgrade() else { return };
        let ready = tokio::select! {
            r = this.fd.writable() => r,
            _ = stop.cancelled() => return,
        };
        let mut guard = match ready {
            Ok(g) => g,
            Err(e) => {
                warn!("usbfs poll failed: {e}");
                this.fail_all(-libc::ENODEV);
                return;
            }
        };
        let result: Result<io::Result<io::Result<()>>, _> = guard.try_io(|_| match this.reap_all() {
            // Everything reaped: report "would block" so the readiness is cleared.
            Ok(()) => Err(io::ErrorKind::WouldBlock.into()),
            Err(e) => Ok(Err(e)),
        });
        if let Ok(Ok(Err(e))) = result {
            debug!("usbfs reap stopped: {e}");
            this.fail_all(-libc::ESHUTDOWN);
            return;
        }
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> io::Result<T> + Send + 'static) -> Result<T, i32> {
    match tokio::time::timeout(Duration::from_secs(10), tokio::task::spawn_blocking(f)).await {
        Ok(Ok(Ok(v))) => Ok(v),
        Ok(Ok(Err(e))) => Err(errno_of(&e)),
        _ => Err(-libc::ETIMEDOUT),
    }
}

// ---------------------------------------------------------------------------
// Isochronous buffer layout helpers.
//
// USB/IP and usbfs lay isochronous data out differently:
// * USB/IP OUT: packet i is at `offset_i` in the transfer buffer (offsets from the host).
// * usbfs:      packets are back to back, packet i at sum(length_0..i).
// * USB/IP IN reply: only the received bytes, back to back (sum of actual lengths).

/// Repack USB/IP OUT data into usbfs layout.
pub fn pack_iso_out(data: &[u8], packets: &[IsoRequest]) -> Vec<u8> {
    let mut out = Vec::with_capacity(packets.iter().map(|p| p.length as usize).sum());
    for p in packets {
        let start = (p.offset as usize).min(data.len());
        let end = (start + p.length as usize).min(data.len());
        out.extend_from_slice(&data[start..end]);
        // Keep packet positions aligned to their lengths even if the host sent less.
        out.resize(out.len() + (p.length as usize - (end - start)), 0);
    }
    out
}

/// Compact usbfs IN data into the USB/IP reply layout.
pub fn compact_iso_in(buf: &[u8], packets: &[IsoRequest], results: &[IsoResult]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for (p, r) in packets.iter().zip(results) {
        let n = (r.actual_length.min(p.length)) as usize;
        let end = (pos + n).min(buf.len());
        out.extend_from_slice(&buf[pos.min(buf.len())..end]);
        pos += p.length as usize;
    }
    out
}

pub fn path_exists(busnum: u8, devnum: u8) -> bool {
    Path::new(&format!("/dev/bus/usb/{busnum:03}/{devnum:03}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layout_matches_kernel_abi() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<UsbdevfsUrb>(), 56);
            assert_eq!(std::mem::size_of::<CtrlTransfer>(), 24);
            assert_eq!(std::mem::size_of::<UsbdevfsIoctl>(), 16);
            // Values from <linux/usbdevice_fs.h> on x86_64 / aarch64.
            assert_eq!(USBDEVFS_SUBMITURB, 0x8038_550a);
            assert_eq!(USBDEVFS_REAPURBNDELAY, 0x4008_550d);
            assert_eq!(USBDEVFS_CONTROL, 0xc018_5500);
            assert_eq!(USBDEVFS_IOCTL, 0xc010_5512);
        }
        assert_eq!(std::mem::size_of::<IsoPacketDesc>(), 12);
        assert_eq!(std::mem::size_of::<DisconnectClaim>(), 264);
        assert_eq!(USBDEVFS_DISCARDURB, 0x550b);
        assert_eq!(USBDEVFS_SETINTERFACE, 0x8008_5504);
        assert_eq!(USBDEVFS_RELEASEINTERFACE, 0x8004_5510);
        assert_eq!(USBDEVFS_CLEAR_HALT, 0x8004_5515);
        assert_eq!(USBDEVFS_CONNECT, 0x5517);
        assert_eq!(USBDEVFS_DISCONNECT_CLAIM, 0x8108_551b);
    }

    #[test]
    fn urb_memory_layout() {
        let mut m = UrbMem::new(3);
        let base = m.header() as usize;
        let iso = m.iso().as_ptr() as usize;
        assert_eq!(iso - base, 56.min(std::mem::size_of::<UsbdevfsUrb>()));
        assert_eq!(m.iso().len(), 3);
        assert_eq!(base % 8, 0);
    }

    #[test]
    fn iso_out_repacking() {
        // Host buffer with gaps: packet 0 at 0 (3 bytes), packet 1 at 8 (2 bytes).
        let data = [1, 2, 3, 0, 0, 0, 0, 0, 9, 9];
        let packets = [
            IsoRequest { offset: 0, length: 3 },
            IsoRequest { offset: 8, length: 2 },
        ];
        assert_eq!(pack_iso_out(&data, &packets), vec![1, 2, 3, 9, 9]);
    }

    #[test]
    fn iso_in_compaction() {
        // usbfs layout: packets of length 4 back to back; actual lengths 2, 0, 3.
        let buf = [1, 2, 0, 0, 7, 7, 7, 7, 5, 6, 7, 0];
        let packets = [
            IsoRequest { offset: 0, length: 4 },
            IsoRequest { offset: 4, length: 4 },
            IsoRequest { offset: 8, length: 4 },
        ];
        let results = [
            IsoResult {
                actual_length: 2,
                status: 0,
            },
            IsoResult {
                actual_length: 0,
                status: 0,
            },
            IsoResult {
                actual_length: 3,
                status: 0,
            },
        ];
        assert_eq!(compact_iso_in(&buf, &packets, &results), vec![1, 2, 5, 6, 7]);
    }
}
