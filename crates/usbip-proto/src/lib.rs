//! USB/IP wire protocol (as implemented by the Linux kernel `usbip` drivers).
//!
//! All multi-byte integers are big-endian (network byte order).
//!
//! The protocol has two phases on a single TCP connection:
//!
//! 1. **Operation phase**: `OP_REQ_DEVLIST` / `OP_REP_DEVLIST` (list exported
//!    devices, connection then closes) or `OP_REQ_IMPORT` / `OP_REP_IMPORT`
//!    (claim one device; on success the connection switches to phase 2).
//! 2. **URB phase**: `USBIP_CMD_SUBMIT`, `USBIP_RET_SUBMIT`, `USBIP_CMD_UNLINK`,
//!    `USBIP_RET_UNLINK`, each with a fixed 48-byte header.
//!
//! Reference: <https://docs.kernel.org/usb/usbip_protocol.html>

use std::fmt;

#[cfg(feature = "tokio")]
mod io;
#[cfg(feature = "tokio")]
pub use io::*;

/// Protocol version spoken by all current Linux kernels.
pub const USBIP_VERSION: u16 = 0x0111;
/// Default TCP port.
pub const USBIP_PORT: u16 = 3240;

pub const OP_REQ_DEVLIST: u16 = 0x8005;
pub const OP_REP_DEVLIST: u16 = 0x0005;
pub const OP_REQ_IMPORT: u16 = 0x8003;
pub const OP_REP_IMPORT: u16 = 0x0003;

pub const USBIP_CMD_SUBMIT: u32 = 0x0001;
pub const USBIP_CMD_UNLINK: u32 = 0x0002;
pub const USBIP_RET_SUBMIT: u32 = 0x0003;
pub const USBIP_RET_UNLINK: u32 = 0x0004;

pub const OP_HEADER_LEN: usize = 8;
pub const URB_HEADER_LEN: usize = 48;
pub const USB_DEVICE_LEN: usize = 312;
pub const USB_INTERFACE_LEN: usize = 4;
pub const ISO_DESC_LEN: usize = 16;
pub const BUSID_LEN: usize = 32;
pub const PATH_LEN: usize = 256;

/// Upper bound we accept for a single transfer buffer. Protects against
/// malicious peers asking us to allocate unbounded memory.
pub const MAX_TRANSFER_LEN: usize = 16 * 1024 * 1024;
/// Upper bound on isochronous packets per URB.
pub const MAX_ISO_PACKETS: usize = 1024;
/// Upper bound on devices in a device list reply.
pub const MAX_DEVICES: usize = 4096;

/// Status values in `OpHeader::status`.
pub const ST_OK: u32 = 0;
pub const ST_NA: u32 = 1; // device not available
pub const ST_DEV_BUSY: u32 = 2;
pub const ST_DEV_ERR: u32 = 3;
pub const ST_NODEV: u32 = 4;
pub const ST_ERROR: u32 = 5;

/// Linux errno values used in URB status fields (sent negated).
pub mod errno {
    pub const ENOENT: i32 = 2;
    pub const EIO: i32 = 5;
    pub const ENOMEM: i32 = 12;
    pub const ENODEV: i32 = 19;
    pub const EINVAL: i32 = 22;
    pub const EPIPE: i32 = 32;
    pub const ENOSYS: i32 = 38;
    pub const EOVERFLOW: i32 = 75;
    pub const ECONNRESET: i32 = 104;
    pub const ESHUTDOWN: i32 = 108;
    pub const ETIMEDOUT: i32 = 110;
    pub const EPROTO: i32 = 71;
    pub const EXDEV: i32 = 18;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported USB/IP version {0:#06x}")]
    Version(u16),
    #[error("unexpected operation code {0:#06x}")]
    UnexpectedOp(u16),
    #[error("unknown URB command {0:#x}")]
    UnknownCommand(u32),
    #[error("{what} too large: {len}")]
    TooLarge { what: &'static str, len: usize },
    #[error("truncated message")]
    Truncated,
    #[error("RET_SUBMIT for unknown seqnum {0}")]
    UnknownSeqnum(u32),
}

pub type Result<T> = std::result::Result<T, Error>;

/// USB device speed as used by USB/IP (matches the kernel `enum usb_device_speed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Speed {
    Unknown = 0,
    Low = 1,
    Full = 2,
    High = 3,
    Wireless = 4,
    Super = 5,
    SuperPlus = 6,
}

impl Speed {
    pub fn from_u32(v: u32) -> Speed {
        match v {
            1 => Speed::Low,
            2 => Speed::Full,
            3 => Speed::High,
            4 => Speed::Wireless,
            5 => Speed::Super,
            6 => Speed::SuperPlus,
            _ => Speed::Unknown,
        }
    }
    /// SuperSpeed devices must be attached to a SuperSpeed root hub port on vhci.
    pub fn is_superspeed(self) -> bool {
        matches!(self, Speed::Super | Speed::SuperPlus)
    }
}

impl fmt::Display for Speed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Speed::Unknown => "unknown",
            Speed::Low => "1.5 Mbit/s (low)",
            Speed::Full => "12 Mbit/s (full)",
            Speed::High => "480 Mbit/s (high)",
            Speed::Wireless => "wireless",
            Speed::Super => "5 Gbit/s (super)",
            Speed::SuperPlus => "10 Gbit/s (super+)",
        };
        f.write_str(s)
    }
}

/// Transfer direction in URB headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Out = 0,
    In = 1,
}

impl Direction {
    pub fn from_u32(v: u32) -> Direction {
        if v == 1 {
            Direction::In
        } else {
            Direction::Out
        }
    }
}

/// `struct usbip_usb_device`: an exported device as described in
/// `OP_REP_DEVLIST` and `OP_REP_IMPORT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub path: String,
    pub busid: String,
    pub busnum: u32,
    pub devnum: u32,
    pub speed: Speed,
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub configuration_value: u8,
    pub num_configurations: u8,
    pub num_interfaces: u8,
}

impl DeviceDescriptor {
    /// The `devid` value used in URB headers and the vhci attach interface.
    pub fn devid(&self) -> u32 {
        (self.busnum << 16) | (self.devnum & 0xffff)
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        put_fixed_str(out, &self.path, PATH_LEN);
        put_fixed_str(out, &self.busid, BUSID_LEN);
        out.extend_from_slice(&self.busnum.to_be_bytes());
        out.extend_from_slice(&self.devnum.to_be_bytes());
        out.extend_from_slice(&(self.speed as u32).to_be_bytes());
        out.extend_from_slice(&self.id_vendor.to_be_bytes());
        out.extend_from_slice(&self.id_product.to_be_bytes());
        out.extend_from_slice(&self.bcd_device.to_be_bytes());
        out.extend_from_slice(&[
            self.device_class,
            self.device_subclass,
            self.device_protocol,
            self.configuration_value,
            self.num_configurations,
            self.num_interfaces,
        ]);
    }

    pub fn decode(b: &[u8]) -> Result<DeviceDescriptor> {
        if b.len() < USB_DEVICE_LEN {
            return Err(Error::Truncated);
        }
        let mut r = Reader(b);
        Ok(DeviceDescriptor {
            path: r.fixed_str(PATH_LEN),
            busid: r.fixed_str(BUSID_LEN),
            busnum: r.u32(),
            devnum: r.u32(),
            speed: Speed::from_u32(r.u32()),
            id_vendor: r.u16(),
            id_product: r.u16(),
            bcd_device: r.u16(),
            device_class: r.u8(),
            device_subclass: r.u8(),
            device_protocol: r.u8(),
            configuration_value: r.u8(),
            num_configurations: r.u8(),
            num_interfaces: r.u8(),
        })
    }
}

/// `struct usbip_usb_interface`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterfaceDescriptor {
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

/// One entry of `OP_REP_DEVLIST`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedDevice {
    pub device: DeviceDescriptor,
    pub interfaces: Vec<InterfaceDescriptor>,
}

/// The 8-byte header of every operation-phase message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpHeader {
    pub version: u16,
    pub code: u16,
    pub status: u32,
}

impl OpHeader {
    pub fn new(code: u16, status: u32) -> Self {
        OpHeader {
            version: USBIP_VERSION,
            code,
            status,
        }
    }
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(&self.code.to_be_bytes());
        out.extend_from_slice(&self.status.to_be_bytes());
    }
    pub fn decode(b: &[u8; OP_HEADER_LEN]) -> OpHeader {
        let mut r = Reader(b);
        OpHeader {
            version: r.u16(),
            code: r.u16(),
            status: r.u32(),
        }
    }
}

/// Encode a complete `OP_REQ_DEVLIST`.
pub fn encode_req_devlist() -> Vec<u8> {
    // The request is only the 8-byte header (kernel `struct op_devlist_request` is empty).
    let mut v = Vec::with_capacity(OP_HEADER_LEN);
    OpHeader::new(OP_REQ_DEVLIST, 0).encode(&mut v);
    v
}

/// Encode a complete `OP_REQ_IMPORT`.
pub fn encode_req_import(busid: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(OP_HEADER_LEN + BUSID_LEN);
    OpHeader::new(OP_REQ_IMPORT, 0).encode(&mut v);
    put_fixed_str(&mut v, busid, BUSID_LEN);
    v
}

/// Encode a complete `OP_REP_DEVLIST`.
pub fn encode_rep_devlist(devices: &[ExportedDevice]) -> Vec<u8> {
    let mut v = Vec::new();
    OpHeader::new(OP_REP_DEVLIST, ST_OK).encode(&mut v);
    v.extend_from_slice(&(devices.len() as u32).to_be_bytes());
    for d in devices {
        d.device.encode(&mut v);
        for i in &d.interfaces {
            v.extend_from_slice(&[i.class, i.subclass, i.protocol, 0]);
        }
    }
    v
}

/// Encode a complete `OP_REP_IMPORT`. `device` is `None` on failure.
pub fn encode_rep_import(status: u32, device: Option<&DeviceDescriptor>) -> Vec<u8> {
    let mut v = Vec::new();
    OpHeader::new(OP_REP_IMPORT, status).encode(&mut v);
    if let (ST_OK, Some(d)) = (status, device) {
        d.encode(&mut v);
    }
    v
}

/// The first 20 bytes of every URB-phase message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UrbHeader {
    pub command: u32,
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
}

/// `struct usbip_iso_packet_descriptor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IsoPacket {
    pub offset: u32,
    pub length: u32,
    pub actual_length: u32,
    pub status: i32,
}

impl IsoPacket {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.offset.to_be_bytes());
        out.extend_from_slice(&self.length.to_be_bytes());
        out.extend_from_slice(&self.actual_length.to_be_bytes());
        out.extend_from_slice(&self.status.to_be_bytes());
    }
    fn decode(b: &[u8]) -> IsoPacket {
        let mut r = Reader(b);
        IsoPacket {
            offset: r.u32(),
            length: r.u32(),
            actual_length: r.u32(),
            status: r.i32(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdSubmit {
    pub seqnum: u32,
    pub devid: u32,
    pub direction: Direction,
    pub ep: u32,
    pub transfer_flags: u32,
    pub transfer_buffer_length: u32,
    pub start_frame: i32,
    /// Raw field; `0` or `0xffffffff` means "not isochronous".
    pub number_of_packets: u32,
    pub interval: i32,
    pub setup: [u8; 8],
    /// OUT data (empty for IN transfers).
    pub data: Vec<u8>,
    pub iso_packets: Vec<IsoPacket>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetSubmit {
    pub seqnum: u32,
    pub status: i32,
    pub actual_length: u32,
    pub start_frame: i32,
    pub number_of_packets: u32,
    pub error_count: i32,
    /// IN data (empty for OUT transfers).
    pub data: Vec<u8>,
    pub iso_packets: Vec<IsoPacket>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmdUnlink {
    pub seqnum: u32,
    pub devid: u32,
    pub direction: Direction,
    pub ep: u32,
    pub unlink_seqnum: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetUnlink {
    pub seqnum: u32,
    pub status: i32,
}

/// Any URB-phase message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrbMessage {
    CmdSubmit(CmdSubmit),
    CmdUnlink(CmdUnlink),
    RetSubmit(RetSubmit),
    RetUnlink(RetUnlink),
}

/// Number of iso packets implied by a raw `number_of_packets` field.
pub fn iso_count(raw: u32) -> usize {
    if raw == 0xffff_ffff {
        0
    } else {
        raw as usize
    }
}

impl UrbMessage {
    pub fn seqnum(&self) -> u32 {
        match self {
            UrbMessage::CmdSubmit(m) => m.seqnum,
            UrbMessage::CmdUnlink(m) => m.seqnum,
            UrbMessage::RetSubmit(m) => m.seqnum,
            UrbMessage::RetUnlink(m) => m.seqnum,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(URB_HEADER_LEN);
        match self {
            UrbMessage::CmdSubmit(m) => {
                put_header(
                    &mut v,
                    USBIP_CMD_SUBMIT,
                    m.seqnum,
                    m.devid,
                    m.direction as u32,
                    m.ep,
                );
                v.extend_from_slice(&m.transfer_flags.to_be_bytes());
                v.extend_from_slice(&m.transfer_buffer_length.to_be_bytes());
                v.extend_from_slice(&m.start_frame.to_be_bytes());
                v.extend_from_slice(&m.number_of_packets.to_be_bytes());
                v.extend_from_slice(&m.interval.to_be_bytes());
                v.extend_from_slice(&m.setup);
                if m.direction == Direction::Out {
                    v.extend_from_slice(&m.data);
                }
                for p in &m.iso_packets {
                    p.encode(&mut v);
                }
            }
            UrbMessage::RetSubmit(m) => {
                // Per the spec devid, direction and ep are zero in replies.
                put_header(&mut v, USBIP_RET_SUBMIT, m.seqnum, 0, 0, 0);
                v.extend_from_slice(&m.status.to_be_bytes());
                v.extend_from_slice(&m.actual_length.to_be_bytes());
                v.extend_from_slice(&m.start_frame.to_be_bytes());
                v.extend_from_slice(&m.number_of_packets.to_be_bytes());
                v.extend_from_slice(&m.error_count.to_be_bytes());
                v.extend_from_slice(&[0u8; 8]);
                v.extend_from_slice(&m.data);
                for p in &m.iso_packets {
                    p.encode(&mut v);
                }
            }
            UrbMessage::CmdUnlink(m) => {
                put_header(
                    &mut v,
                    USBIP_CMD_UNLINK,
                    m.seqnum,
                    m.devid,
                    m.direction as u32,
                    m.ep,
                );
                v.extend_from_slice(&m.unlink_seqnum.to_be_bytes());
                v.extend_from_slice(&[0u8; 24]);
            }
            UrbMessage::RetUnlink(m) => {
                put_header(&mut v, USBIP_RET_UNLINK, m.seqnum, 0, 0, 0);
                v.extend_from_slice(&m.status.to_be_bytes());
                v.extend_from_slice(&[0u8; 24]);
            }
        }
        v
    }

    /// Parse the fixed header and report how many trailing bytes follow it.
    ///
    /// `in_dir_for` is consulted for `RET_SUBMIT`, whose header does not carry
    /// the direction: it must return the direction of the original
    /// `CMD_SUBMIT` with that seqnum (only IN replies carry data).
    pub fn parse_header(
        hdr: &[u8; URB_HEADER_LEN],
        in_dir_for: &mut dyn FnMut(u32) -> Option<Direction>,
    ) -> Result<PendingUrb> {
        let mut r = Reader(hdr);
        let h = UrbHeader {
            command: r.u32(),
            seqnum: r.u32(),
            devid: r.u32(),
            direction: r.u32(),
            ep: r.u32(),
        };
        let body: [u8; 28] = hdr[20..48].try_into().unwrap();
        let mut b = Reader(&body);
        match h.command {
            USBIP_CMD_SUBMIT => {
                let transfer_flags = b.u32();
                let transfer_buffer_length = b.u32();
                let start_frame = b.i32();
                let number_of_packets = b.u32();
                let interval = b.i32();
                let setup: [u8; 8] = body[20..28].try_into().unwrap();
                let direction = Direction::from_u32(h.direction);
                let tlen = transfer_buffer_length as usize;
                if tlen > MAX_TRANSFER_LEN {
                    return Err(Error::TooLarge {
                        what: "transfer buffer",
                        len: tlen,
                    });
                }
                let n_iso = iso_count(number_of_packets);
                if n_iso > MAX_ISO_PACKETS {
                    return Err(Error::TooLarge {
                        what: "iso packet count",
                        len: n_iso,
                    });
                }
                let data_len = if direction == Direction::Out { tlen } else { 0 };
                Ok(PendingUrb {
                    data_len,
                    iso_len: n_iso * ISO_DESC_LEN,
                    msg: UrbMessage::CmdSubmit(CmdSubmit {
                        seqnum: h.seqnum,
                        devid: h.devid,
                        direction,
                        ep: h.ep,
                        transfer_flags,
                        transfer_buffer_length,
                        start_frame,
                        number_of_packets,
                        interval,
                        setup,
                        data: Vec::new(),
                        iso_packets: Vec::new(),
                    }),
                })
            }
            USBIP_RET_SUBMIT => {
                let status = b.i32();
                let actual_length = b.u32();
                let start_frame = b.i32();
                let number_of_packets = b.u32();
                let error_count = b.i32();
                let dir = in_dir_for(h.seqnum).ok_or(Error::UnknownSeqnum(h.seqnum))?;
                let alen = actual_length as usize;
                if alen > MAX_TRANSFER_LEN {
                    return Err(Error::TooLarge {
                        what: "actual length",
                        len: alen,
                    });
                }
                let n_iso = iso_count(number_of_packets);
                if n_iso > MAX_ISO_PACKETS {
                    return Err(Error::TooLarge {
                        what: "iso packet count",
                        len: n_iso,
                    });
                }
                Ok(PendingUrb {
                    data_len: if dir == Direction::In { alen } else { 0 },
                    iso_len: n_iso * ISO_DESC_LEN,
                    msg: UrbMessage::RetSubmit(RetSubmit {
                        seqnum: h.seqnum,
                        status,
                        actual_length,
                        start_frame,
                        number_of_packets,
                        error_count,
                        data: Vec::new(),
                        iso_packets: Vec::new(),
                    }),
                })
            }
            USBIP_CMD_UNLINK => Ok(PendingUrb {
                data_len: 0,
                iso_len: 0,
                msg: UrbMessage::CmdUnlink(CmdUnlink {
                    seqnum: h.seqnum,
                    devid: h.devid,
                    direction: Direction::from_u32(h.direction),
                    ep: h.ep,
                    unlink_seqnum: b.u32(),
                }),
            }),
            USBIP_RET_UNLINK => Ok(PendingUrb {
                data_len: 0,
                iso_len: 0,
                msg: UrbMessage::RetUnlink(RetUnlink {
                    seqnum: h.seqnum,
                    status: b.i32(),
                }),
            }),
            other => Err(Error::UnknownCommand(other)),
        }
    }
}

/// A URB message whose fixed header has been parsed but whose payload
/// (data followed by iso descriptors) has not been read yet.
#[derive(Debug)]
pub struct PendingUrb {
    pub data_len: usize,
    pub iso_len: usize,
    pub msg: UrbMessage,
}

impl PendingUrb {
    pub fn payload_len(&self) -> usize {
        self.data_len + self.iso_len
    }

    /// Attach the payload bytes (exactly `payload_len()` of them).
    pub fn finish(mut self, payload: Vec<u8>) -> Result<UrbMessage> {
        if payload.len() != self.payload_len() {
            return Err(Error::Truncated);
        }
        let mut data = payload;
        let iso_bytes = data.split_off(self.data_len);
        let iso: Vec<IsoPacket> = iso_bytes
            .chunks_exact(ISO_DESC_LEN)
            .map(IsoPacket::decode)
            .collect();
        match &mut self.msg {
            UrbMessage::CmdSubmit(m) => {
                m.data = data;
                m.iso_packets = iso;
            }
            UrbMessage::RetSubmit(m) => {
                m.data = data;
                m.iso_packets = iso;
            }
            _ => {}
        }
        Ok(self.msg)
    }
}

/// Decode a complete URB message from a byte slice (mainly for tests).
pub fn decode_urb(bytes: &[u8], in_dir_for: &mut dyn FnMut(u32) -> Option<Direction>) -> Result<UrbMessage> {
    if bytes.len() < URB_HEADER_LEN {
        return Err(Error::Truncated);
    }
    let hdr: &[u8; URB_HEADER_LEN] = bytes[..URB_HEADER_LEN].try_into().unwrap();
    let p = UrbMessage::parse_header(hdr, in_dir_for)?;
    let rest = &bytes[URB_HEADER_LEN..];
    if rest.len() != p.payload_len() {
        return Err(Error::Truncated);
    }
    p.finish(rest.to_vec())
}

/// Decode the interfaces that follow a device in `OP_REP_DEVLIST`.
pub fn decode_interfaces(b: &[u8]) -> Vec<InterfaceDescriptor> {
    b.chunks_exact(USB_INTERFACE_LEN)
        .map(|c| InterfaceDescriptor {
            class: c[0],
            subclass: c[1],
            protocol: c[2],
        })
        .collect()
}

fn put_header(v: &mut Vec<u8>, cmd: u32, seq: u32, devid: u32, dir: u32, ep: u32) {
    for x in [cmd, seq, devid, dir, ep] {
        v.extend_from_slice(&x.to_be_bytes());
    }
}

fn put_fixed_str(out: &mut Vec<u8>, s: &str, len: usize) {
    let bytes = s.as_bytes();
    // Always leave room for a NUL terminator.
    let n = bytes.len().min(len - 1);
    out.extend_from_slice(&bytes[..n]);
    out.resize(out.len() + (len - n), 0);
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let (a, b) = self.0.split_at(N);
        self.0 = b;
        a.try_into().unwrap()
    }
    fn u8(&mut self) -> u8 {
        self.take::<1>()[0]
    }
    fn u16(&mut self) -> u16 {
        u16::from_be_bytes(self.take())
    }
    fn u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take())
    }
    fn i32(&mut self) -> i32 {
        i32::from_be_bytes(self.take())
    }
    fn fixed_str(&mut self, len: usize) -> String {
        let (a, b) = self.0.split_at(len);
        self.0 = b;
        let end = a.iter().position(|&c| c == 0).unwrap_or(len);
        String::from_utf8_lossy(&a[..end]).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_device() -> DeviceDescriptor {
        DeviceDescriptor {
            path: "/sys/devices/pci0000:00/0000:00:1d.1/usb2/2-1".into(),
            busid: "2-1".into(),
            busnum: 2,
            devnum: 3,
            speed: Speed::High,
            id_vendor: 0x1d6b,
            id_product: 0x0104,
            bcd_device: 0x0100,
            device_class: 0,
            device_subclass: 0,
            device_protocol: 0,
            configuration_value: 1,
            num_configurations: 1,
            num_interfaces: 2,
        }
    }

    #[test]
    fn device_roundtrip_and_size() {
        let d = sample_device();
        let mut v = Vec::new();
        d.encode(&mut v);
        assert_eq!(v.len(), USB_DEVICE_LEN);
        assert_eq!(DeviceDescriptor::decode(&v).unwrap(), d);
        assert_eq!(d.devid(), 0x0002_0003);
    }

    #[test]
    fn devlist_layout() {
        let devs = vec![ExportedDevice {
            device: sample_device(),
            interfaces: vec![
                InterfaceDescriptor {
                    class: 8,
                    subclass: 6,
                    protocol: 0x50,
                },
                InterfaceDescriptor {
                    class: 3,
                    subclass: 0,
                    protocol: 0,
                },
            ],
        }];
        let v = encode_rep_devlist(&devs);
        assert_eq!(v.len(), 8 + 4 + USB_DEVICE_LEN + 2 * USB_INTERFACE_LEN);
        assert_eq!(&v[..4], &[0x01, 0x11, 0x00, 0x05]);
        assert_eq!(&v[8..12], &[0, 0, 0, 1]);
    }

    #[test]
    fn import_request_layout() {
        let v = encode_req_import("1-1.2");
        assert_eq!(v.len(), 40);
        assert_eq!(&v[..4], &[0x01, 0x11, 0x80, 0x03]);
        assert_eq!(&v[8..13], b"1-1.2");
        assert!(v[13..].iter().all(|&b| b == 0));
    }

    #[test]
    fn long_busid_is_truncated_with_nul() {
        let long = "x".repeat(100);
        let v = encode_req_import(&long);
        assert_eq!(v.len(), 40);
        assert_eq!(v[39], 0);
    }

    #[test]
    fn cmd_submit_out_roundtrip() {
        let m = UrbMessage::CmdSubmit(CmdSubmit {
            seqnum: 7,
            devid: 0x10002,
            direction: Direction::Out,
            ep: 1,
            transfer_flags: 0,
            transfer_buffer_length: 5,
            start_frame: 0,
            number_of_packets: 0,
            interval: 0,
            setup: [0; 8],
            data: b"hello".to_vec(),
            iso_packets: vec![],
        });
        let bytes = m.encode();
        assert_eq!(bytes.len(), 48 + 5);
        assert_eq!(decode_urb(&bytes, &mut |_| None).unwrap(), m);
    }

    #[test]
    fn cmd_submit_in_has_no_payload() {
        let m = UrbMessage::CmdSubmit(CmdSubmit {
            seqnum: 1,
            devid: 1,
            direction: Direction::In,
            ep: 0,
            transfer_flags: 0x200,
            transfer_buffer_length: 18,
            start_frame: 0,
            number_of_packets: 0xffff_ffff,
            interval: 0,
            setup: [0x80, 6, 0, 1, 0, 0, 18, 0],
            data: vec![],
            iso_packets: vec![],
        });
        let bytes = m.encode();
        assert_eq!(bytes.len(), 48);
        // setup packet sits at offset 40
        assert_eq!(&bytes[40..48], &[0x80, 6, 0, 1, 0, 0, 18, 0]);
        assert_eq!(decode_urb(&bytes, &mut |_| None).unwrap(), m);
    }

    #[test]
    fn ret_submit_needs_direction() {
        let m = UrbMessage::RetSubmit(RetSubmit {
            seqnum: 9,
            status: 0,
            actual_length: 3,
            start_frame: 0,
            number_of_packets: 0,
            error_count: 0,
            data: vec![1, 2, 3],
            iso_packets: vec![],
        });
        let bytes = m.encode();
        assert!(matches!(
            decode_urb(&bytes, &mut |_| None),
            Err(Error::UnknownSeqnum(9))
        ));
        assert_eq!(decode_urb(&bytes, &mut |_| Some(Direction::In)).unwrap(), m);
    }

    #[test]
    fn iso_roundtrip() {
        let m = UrbMessage::RetSubmit(RetSubmit {
            seqnum: 2,
            status: 0,
            actual_length: 4,
            start_frame: 100,
            number_of_packets: 2,
            error_count: 0,
            data: vec![9, 9, 8, 8],
            iso_packets: vec![
                IsoPacket {
                    offset: 0,
                    length: 192,
                    actual_length: 2,
                    status: 0,
                },
                IsoPacket {
                    offset: 192,
                    length: 192,
                    actual_length: 2,
                    status: 0,
                },
            ],
        });
        let bytes = m.encode();
        assert_eq!(bytes.len(), 48 + 4 + 32);
        assert_eq!(decode_urb(&bytes, &mut |_| Some(Direction::In)).unwrap(), m);
    }

    #[test]
    fn unlink_roundtrip() {
        let c = UrbMessage::CmdUnlink(CmdUnlink {
            seqnum: 11,
            devid: 5,
            direction: Direction::In,
            ep: 1,
            unlink_seqnum: 10,
        });
        let b = c.encode();
        assert_eq!(b.len(), 48);
        assert_eq!(decode_urb(&b, &mut |_| None).unwrap(), c);
        let r = UrbMessage::RetUnlink(RetUnlink {
            seqnum: 11,
            status: -errno::ECONNRESET,
        });
        assert_eq!(decode_urb(&r.encode(), &mut |_| None).unwrap(), r);
    }

    #[test]
    fn rejects_huge_buffers() {
        let mut hdr = [0u8; 48];
        hdr[3] = 1; // CMD_SUBMIT
        hdr[15] = 0; // OUT
        hdr[24..28].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            UrbMessage::parse_header(&hdr, &mut |_| None),
            Err(Error::TooLarge { .. })
        ));
    }

    #[test]
    fn rejects_unknown_command() {
        let mut hdr = [0u8; 48];
        hdr[3] = 0x42;
        assert!(matches!(
            UrbMessage::parse_header(&hdr, &mut |_| None),
            Err(Error::UnknownCommand(0x42))
        ));
    }

    /// Simple deterministic fuzz: random bytes must never panic the decoder.
    #[test]
    fn random_headers_do_not_panic() {
        let mut x: u64 = 0x1234_5678_9abc_def0;
        for _ in 0..20_000 {
            let mut hdr = [0u8; 48];
            for b in hdr.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
            hdr[0..3].fill(0);
            hdr[3] = (x % 5) as u8;
            let _ = UrbMessage::parse_header(&hdr, &mut |_| Some(Direction::In));
            let _ = DeviceDescriptor::decode(&[hdr.as_slice(); 7].concat());
        }
    }
}
