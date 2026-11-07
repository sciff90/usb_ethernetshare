//! USB mass-storage device (Bulk-Only Transport, SCSI transparent command
//! set) backed by a RAM disk. A Linux client sees it as `/dev/sdX` and can
//! partition, format and mount it. Contents survive detach/re-attach for the
//! lifetime of the server process.

use std::sync::Mutex;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use usbip_proto::Speed;

use super::{DeviceSpec, EndpointSpec, InterfaceSpec, MockFunction, MOCK_VID, PID_STORAGE};
use crate::device::{Setup, UrbRequest, UrbResponse};

pub const BLOCK_SIZE: usize = 512;
const CBW_SIGNATURE: u32 = 0x4342_5355; // "USBC"
const CSW_SIGNATURE: u32 = 0x5342_5355; // "USBS"

pub const EP_OUT: u8 = 0x02;
pub const EP_IN: u8 = 0x81;

// Sense keys / additional sense codes.
const NO_SENSE: (u8, u8, u8) = (0, 0, 0);
const INVALID_COMMAND: (u8, u8, u8) = (0x05, 0x20, 0x00);
const INVALID_FIELD: (u8, u8, u8) = (0x05, 0x24, 0x00);
const LBA_OUT_OF_RANGE: (u8, u8, u8) = (0x05, 0x21, 0x00);
const MEDIUM_NOT_PRESENT: (u8, u8, u8) = (0x02, 0x3a, 0x00);

#[derive(Debug)]
enum Phase {
    /// Waiting for a Command Block Wrapper.
    Idle,
    /// Sending data to the host, then the CSW.
    DataIn { data: Vec<u8>, sent: usize },
    /// Receiving data from the host.
    DataOut {
        remaining: usize,
        received: Vec<u8>,
        write_lba: Option<u64>,
    },
    /// Data phase finished; next IN transfer returns the CSW.
    Status,
}

struct Bot {
    phase: Phase,
    tag: u32,
    expected: u32,
    transferred: u32,
    status: u8,
    sense: (u8, u8, u8),
}

pub struct StorageDevice {
    spec: DeviceSpec,
    disk: Mutex<Vec<u8>>,
    bot: Mutex<Bot>,
    /// Medium ejected with START STOP UNIT (LoEj=1, Start=0), like a real stick after
    /// "safely remove". Media-access commands then fail with MEDIUM NOT PRESENT.
    ejected: std::sync::atomic::AtomicBool,
}

impl StorageDevice {
    pub fn new(serial: String, size_mib: u32) -> Self {
        StorageDevice {
            spec: DeviceSpec {
                vid: MOCK_VID,
                pid: PID_STORAGE,
                bcd_device: 0x0100,
                speed: Speed::High,
                manufacturer: "usbshare".into(),
                product: "usbshare RAM Disk".into(),
                serial,
                interfaces: vec![InterfaceSpec {
                    class: 0x08,    // mass storage
                    subclass: 0x06, // SCSI transparent
                    protocol: 0x50, // bulk-only
                    extra: vec![],
                    endpoints: vec![
                        EndpointSpec {
                            address: EP_IN,
                            attributes: 2,
                            max_packet: 512,
                            interval: 0,
                        },
                        EndpointSpec {
                            address: EP_OUT,
                            attributes: 2,
                            max_packet: 512,
                            interval: 0,
                        },
                    ],
                }],
            },
            disk: Mutex::new(vec![0u8; size_mib as usize * 1024 * 1024]),
            bot: Mutex::new(Bot {
                phase: Phase::Idle,
                tag: 0,
                expected: 0,
                transferred: 0,
                status: 0,
                sense: NO_SENSE,
            }),
            ejected: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn blocks(&self) -> u64 {
        (self.disk.lock().unwrap().len() / BLOCK_SIZE) as u64
    }

    /// Handle a CBW and set up the following phase.
    fn command(&self, bot: &mut Bot, cbw: &[u8]) -> Result<(), ()> {
        if cbw.len() != 31 || u32::from_le_bytes(cbw[0..4].try_into().unwrap()) != CBW_SIGNATURE {
            return Err(());
        }
        bot.tag = u32::from_le_bytes(cbw[4..8].try_into().unwrap());
        bot.expected = u32::from_le_bytes(cbw[8..12].try_into().unwrap());
        bot.transferred = 0;
        let dir_in = cbw[12] & 0x80 != 0;
        let cb_len = (cbw[14] & 0x1f) as usize;
        let cb = &cbw[15..15 + cb_len.min(16)];
        let expected = bot.expected as usize;

        let result = self.scsi(bot, cb, dir_in);
        match result {
            Scsi::DataIn(mut data) => {
                bot.status = 0;
                bot.sense = NO_SENSE;
                data.truncate(expected);
                bot.phase = if expected == 0 {
                    Phase::Status
                } else {
                    Phase::DataIn { data, sent: 0 }
                };
            }
            Scsi::DataOut(lba) => {
                bot.status = 0;
                bot.sense = NO_SENSE;
                bot.phase = Phase::DataOut {
                    remaining: expected,
                    received: Vec::new(),
                    write_lba: Some(lba),
                };
            }
            Scsi::NoData => {
                bot.status = 0;
                bot.sense = NO_SENSE;
                bot.phase = self.after_command(expected, dir_in);
            }
            Scsi::Fail(sense) => {
                bot.status = 1;
                bot.sense = sense;
                bot.phase = self.after_command(expected, dir_in);
            }
        }
        Ok(())
    }

    /// Phase following a command that produced no data of its own.
    fn after_command(&self, expected: usize, dir_in: bool) -> Phase {
        match (expected, dir_in) {
            (0, _) => Phase::Status,
            // Terminate the data stage with a zero-length packet.
            (_, true) => Phase::DataIn {
                data: Vec::new(),
                sent: 0,
            },
            // Swallow and discard the host's data.
            (n, false) => Phase::DataOut {
                remaining: n,
                received: Vec::new(),
                write_lba: None,
            },
        }
    }

    fn scsi(&self, bot: &Bot, cb: &[u8], dir_in: bool) -> Scsi {
        let Some(&op) = cb.first() else {
            return Scsi::Fail(INVALID_COMMAND);
        };
        let blocks = self.blocks();
        use std::sync::atomic::Ordering::SeqCst;
        // TEST UNIT READY, READ FORMAT CAPACITIES, READ CAPACITY, READ, WRITE, SYNC, VERIFY.
        if matches!(
            op,
            0x00 | 0x23 | 0x25 | 0x9e | 0x28 | 0x88 | 0x2a | 0x8a | 0x35 | 0x2f
        ) && self.ejected.load(SeqCst)
        {
            return Scsi::Fail(MEDIUM_NOT_PRESENT);
        }
        match op {
            0x1b => {
                // START STOP UNIT: byte 4 bit 0 = START, bit 1 = LOEJ.
                let flags = cb.get(4).copied().unwrap_or(0);
                if flags & 0x02 != 0 {
                    self.ejected.store(flags & 0x01 == 0, SeqCst);
                }
                Scsi::NoData
            }
            0x00 | 0x1e | 0x35 | 0x2f => Scsi::NoData, // TEST UNIT READY, PREVENT ALLOW, SYNC CACHE, VERIFY
            0x03 => {
                // REQUEST SENSE
                let (key, asc, ascq) = bot.sense;
                let mut s = vec![0u8; 18];
                s[0] = 0x70;
                s[2] = key;
                s[7] = 10;
                s[12] = asc;
                s[13] = ascq;
                Scsi::DataIn(s)
            }
            0x12 => {
                // INQUIRY
                if cb.len() > 1 && cb[1] & 1 != 0 {
                    return match cb.get(2) {
                        Some(0x00) => Scsi::DataIn(vec![0, 0, 0, 1, 0x00]),
                        _ => Scsi::Fail(INVALID_FIELD),
                    };
                }
                let mut d = vec![0x00, 0x80, 0x04, 0x02, 31, 0, 0, 0];
                d.extend_from_slice(b"usbshare");
                d.extend_from_slice(b"RAM Disk        ");
                d.extend_from_slice(b"0.1 ");
                Scsi::DataIn(d)
            }
            0x1a => Scsi::DataIn(vec![3, 0, 0, 0]), // MODE SENSE(6)
            0x5a => Scsi::DataIn(vec![0, 6, 0, 0, 0, 0, 0, 0]), // MODE SENSE(10)
            0x23 => {
                // READ FORMAT CAPACITIES
                let mut d = vec![0, 0, 0, 8];
                d.extend_from_slice(&(blocks as u32).to_be_bytes());
                d.extend_from_slice(&[0x02, 0, (BLOCK_SIZE >> 8) as u8, BLOCK_SIZE as u8]);
                Scsi::DataIn(d)
            }
            0x25 => {
                // READ CAPACITY(10)
                let mut d = ((blocks - 1) as u32).to_be_bytes().to_vec();
                d.extend_from_slice(&(BLOCK_SIZE as u32).to_be_bytes());
                Scsi::DataIn(d)
            }
            0x9e if cb.get(1).map(|b| b & 0x1f) == Some(0x10) => {
                // READ CAPACITY(16)
                let mut d = (blocks - 1).to_be_bytes().to_vec();
                d.extend_from_slice(&(BLOCK_SIZE as u32).to_be_bytes());
                d.resize(32, 0);
                Scsi::DataIn(d)
            }
            0x28 | 0x88 => {
                // READ(10) / READ(16)
                let Some((lba, count)) = rw_args(cb) else {
                    return Scsi::Fail(INVALID_FIELD);
                };
                if !dir_in || lba + count > blocks {
                    return Scsi::Fail(LBA_OUT_OF_RANGE);
                }
                let disk = self.disk.lock().unwrap();
                let (a, b) = (lba as usize * BLOCK_SIZE, (lba + count) as usize * BLOCK_SIZE);
                Scsi::DataIn(disk[a..b].to_vec())
            }
            0x2a | 0x8a => {
                // WRITE(10) / WRITE(16)
                let Some((lba, count)) = rw_args(cb) else {
                    return Scsi::Fail(INVALID_FIELD);
                };
                if dir_in || lba + count > blocks || count as usize * BLOCK_SIZE != bot.expected as usize {
                    return Scsi::Fail(LBA_OUT_OF_RANGE);
                }
                Scsi::DataOut(lba)
            }
            _ => Scsi::Fail(INVALID_COMMAND),
        }
    }

    fn csw(bot: &mut Bot) -> Vec<u8> {
        let residue = bot.expected.saturating_sub(bot.transferred);
        let mut c = CSW_SIGNATURE.to_le_bytes().to_vec();
        c.extend_from_slice(&bot.tag.to_le_bytes());
        c.extend_from_slice(&residue.to_le_bytes());
        c.push(bot.status);
        bot.phase = Phase::Idle;
        c
    }
}

enum Scsi {
    DataIn(Vec<u8>),
    DataOut(u64),
    NoData,
    Fail((u8, u8, u8)),
}

fn rw_args(cb: &[u8]) -> Option<(u64, u64)> {
    match cb[0] {
        0x28 | 0x2a if cb.len() >= 10 => Some((
            u32::from_be_bytes(cb[2..6].try_into().ok()?) as u64,
            u16::from_be_bytes(cb[7..9].try_into().ok()?) as u64,
        )),
        0x88 | 0x8a if cb.len() >= 16 => Some((
            u64::from_be_bytes(cb[2..10].try_into().ok()?),
            u32::from_be_bytes(cb[10..14].try_into().ok()?) as u64,
        )),
        _ => None,
    }
}

#[async_trait]
impl MockFunction for StorageDevice {
    fn spec(&self) -> &DeviceSpec {
        &self.spec
    }

    async fn control(&self, s: Setup, _data: &[u8]) -> Option<UrbResponse> {
        match (s.request_type, s.request) {
            (0xa1, 0xfe) => Some(UrbResponse::in_data(vec![0])), // GET MAX LUN
            (0x21, 0xff) => {
                // BULK-ONLY MASS STORAGE RESET
                self.reset();
                Some(UrbResponse::out_ok(0))
            }
            _ => None,
        }
    }

    async fn transfer(&self, req: UrbRequest, _cancel: &CancellationToken) -> UrbResponse {
        let mut bot = self.bot.lock().unwrap();
        match req.endpoint {
            EP_OUT => {
                if let Phase::DataOut {
                    remaining,
                    received,
                    write_lba,
                } = &mut bot.phase
                {
                    let take = req.data.len().min(*remaining);
                    received.extend_from_slice(&req.data[..take]);
                    *remaining -= take;
                    if *remaining == 0 {
                        if let Some(lba) = *write_lba {
                            let mut disk = self.disk.lock().unwrap();
                            let a = lba as usize * BLOCK_SIZE;
                            disk[a..a + received.len()].copy_from_slice(received);
                        }
                        bot.phase = Phase::Status;
                    }
                    bot.transferred += take as u32;
                    return UrbResponse::out_ok(req.data.len());
                }
                // Anything else on OUT is a new command.
                match self.command(&mut bot, &req.data) {
                    Ok(()) => UrbResponse::out_ok(req.data.len()),
                    Err(()) => UrbResponse::stall(),
                }
            }
            EP_IN => match &mut bot.phase {
                Phase::DataIn { data, sent } => {
                    let n = (data.len() - *sent).min(req.length as usize);
                    let chunk = data[*sent..*sent + n].to_vec();
                    *sent += n;
                    let done = *sent == data.len();
                    bot.transferred += n as u32;
                    if done {
                        bot.phase = Phase::Status;
                    }
                    UrbResponse::in_data(chunk)
                }
                Phase::Status => UrbResponse::in_data(Self::csw(&mut bot)),
                // Host reads when we expect a command or data: protocol error.
                _ => UrbResponse::stall(),
            },
            _ => UrbResponse::stall(),
        }
    }

    fn reset(&self) {
        let mut bot = self.bot.lock().unwrap();
        bot.phase = Phase::Idle;
        bot.status = 0;
        bot.sense = NO_SENSE;
    }
}
