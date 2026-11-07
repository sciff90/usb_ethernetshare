//! A minimal user-space USB host speaking USB/IP directly.
//!
//! This does not make the device visible to the operating system; it lets
//! the client inspect and exercise a remote device without any kernel
//! support (used by `usbshare info` and `usbshare test`, and by CI).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail};
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use usbip_proto::{self as proto, CmdSubmit, CmdUnlink, Direction, RetSubmit, UrbMessage};

type Waiters = Arc<Mutex<HashMap<u32, (Direction, oneshot::Sender<UrbMessage>)>>>;

pub struct UsbIpHost {
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    waiters: Waiters,
    seq: AtomicU32,
    devid: u32,
    reader: tokio::task::JoinHandle<()>,
}

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("endpoint stalled")]
    Stall,
    #[error("timed out (URB unlinked)")]
    Timeout,
    #[error("transfer failed with status {0}")]
    Status(i32),
    #[error("connection lost")]
    Disconnected,
}

impl Drop for UsbIpHost {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl UsbIpHost {
    /// Take over a stream that has completed `OP_REP_IMPORT`.
    pub fn new(stream: TcpStream, devid: u32) -> Self {
        let (mut rd, wr) = stream.into_split();
        let waiters: Waiters = Arc::new(Mutex::new(HashMap::new()));
        let w = waiters.clone();
        let reader = tokio::spawn(async move {
            loop {
                let msg = {
                    let w2 = w.clone();
                    let mut dir_of = move |seq: u32| w2.lock().unwrap().get(&seq).map(|(d, _)| *d);
                    proto::read_urb(&mut rd, &mut dir_of).await
                };
                let Ok(msg) = msg else { break };
                if let Some((_, tx)) = w.lock().unwrap().remove(&msg.seqnum()) {
                    let _ = tx.send(msg);
                }
            }
            w.lock().unwrap().clear(); // wakes everyone with Disconnected
        });
        UsbIpHost {
            writer: tokio::sync::Mutex::new(wr),
            waiters,
            seq: AtomicU32::new(1),
            devid,
            reader,
        }
    }

    fn next_seq(&self) -> u32 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    async fn send(
        &self,
        msg: UrbMessage,
        dir: Direction,
    ) -> Result<oneshot::Receiver<UrbMessage>, TransferError> {
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().unwrap().insert(msg.seqnum(), (dir, tx));
        let bytes = msg.encode();
        let mut w = self.writer.lock().await;
        w.write_all(&bytes)
            .await
            .map_err(|_| TransferError::Disconnected)?;
        Ok(rx)
    }

    /// Submit a URB and wait for completion, unlinking it after `timeout`.
    pub async fn submit(
        &self,
        ep: u8,
        setup: [u8; 8],
        data: Vec<u8>,
        length: u32,
        timeout: Duration,
    ) -> Result<RetSubmit, TransferError> {
        let direction = if ep & 0x80 != 0 {
            Direction::In
        } else {
            Direction::Out
        };
        let seqnum = self.next_seq();
        let cmd = CmdSubmit {
            seqnum,
            devid: self.devid,
            direction,
            ep: (ep & 0x0f) as u32,
            transfer_flags: 0,
            transfer_buffer_length: if direction == Direction::Out {
                data.len() as u32
            } else {
                length
            },
            start_frame: 0,
            number_of_packets: 0,
            interval: 0,
            setup,
            data,
            iso_packets: vec![],
        };
        let mut rx = self.send(UrbMessage::CmdSubmit(cmd), direction).await?;
        let msg = match tokio::time::timeout(timeout, &mut rx).await {
            Ok(Ok(m)) => m,
            Ok(Err(_)) => return Err(TransferError::Disconnected),
            Err(_) => {
                // Cancel on the server, then see which reply wins the race.
                let useq = self.next_seq();
                let unlink = UrbMessage::CmdUnlink(CmdUnlink {
                    seqnum: useq,
                    devid: self.devid,
                    direction,
                    ep: (ep & 0x0f) as u32,
                    unlink_seqnum: seqnum,
                });
                let urx = self.send(unlink, Direction::Out).await?;
                let r = tokio::time::timeout(Duration::from_secs(5), urx)
                    .await
                    .map_err(|_| TransferError::Disconnected)?
                    .map_err(|_| TransferError::Disconnected)?;
                match r {
                    UrbMessage::RetUnlink(u) if u.status == -proto::errno::ECONNRESET => {
                        self.waiters.lock().unwrap().remove(&seqnum);
                        return Err(TransferError::Timeout);
                    }
                    // Completed just before the unlink arrived; the RET_SUBMIT is on its way.
                    _ => tokio::time::timeout(Duration::from_secs(5), rx)
                        .await
                        .map_err(|_| TransferError::Disconnected)?
                        .map_err(|_| TransferError::Disconnected)?,
                }
            }
        };
        match msg {
            UrbMessage::RetSubmit(r) if r.status == 0 => Ok(r),
            UrbMessage::RetSubmit(r) if r.status == -proto::errno::EPIPE => Err(TransferError::Stall),
            UrbMessage::RetSubmit(r) => Err(TransferError::Status(r.status)),
            _ => Err(TransferError::Disconnected),
        }
    }

    pub async fn control_in(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
    ) -> Result<Vec<u8>, TransferError> {
        let setup = setup_bytes(request_type | 0x80, request, value, index, length);
        Ok(self
            .submit(0x80, setup, vec![], length as u32, Duration::from_secs(5))
            .await?
            .data)
    }

    pub async fn control_out(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &[u8],
    ) -> Result<(), TransferError> {
        let setup = setup_bytes(request_type & 0x7f, request, value, index, data.len() as u16);
        self.submit(0x00, setup, data.to_vec(), 0, Duration::from_secs(5))
            .await?;
        Ok(())
    }

    pub async fn read(&self, ep: u8, length: u32, timeout: Duration) -> Result<Vec<u8>, TransferError> {
        Ok(self
            .submit(ep | 0x80, [0; 8], vec![], length, timeout)
            .await?
            .data)
    }

    pub async fn write(&self, ep: u8, data: &[u8], timeout: Duration) -> Result<u32, TransferError> {
        Ok(self
            .submit(ep & 0x7f, [0; 8], data.to_vec(), 0, timeout)
            .await?
            .actual_length)
    }

    pub async fn device_descriptor(&self) -> anyhow::Result<DeviceDesc> {
        let d = self.control_in(0x00, 6, 0x0100, 0, 18).await?;
        DeviceDesc::parse(&d)
    }

    pub async fn config_descriptor(&self) -> anyhow::Result<Vec<u8>> {
        let head = self.control_in(0x00, 6, 0x0200, 0, 9).await?;
        if head.len() < 9 || head[1] != 2 {
            bail!("bad configuration descriptor header");
        }
        let total = u16::from_le_bytes([head[2], head[3]]);
        Ok(self.control_in(0x00, 6, 0x0200, 0, total).await?)
    }

    pub async fn string(&self, index: u8) -> anyhow::Result<Option<String>> {
        if index == 0 {
            return Ok(None);
        }
        let d = self
            .control_in(0x00, 6, 0x0300 | index as u16, 0x0409, 255)
            .await?;
        if d.len() < 2 || d[1] != 3 {
            return Ok(None);
        }
        let units: Vec<u16> = d[2..(d[0] as usize).min(d.len())]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Ok(Some(String::from_utf16_lossy(&units)))
    }

    pub async fn set_configuration(&self, value: u8) -> anyhow::Result<()> {
        Ok(self.control_out(0x00, 9, value as u16, 0, &[]).await?)
    }
}

pub fn setup_bytes(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> [u8; 8] {
    let v = value.to_le_bytes();
    let i = index.to_le_bytes();
    let l = length.to_le_bytes();
    [request_type, request, v[0], v[1], i[0], i[1], l[0], l[1]]
}

#[derive(Debug, Clone)]
pub struct DeviceDesc {
    pub bcd_usb: u16,
    pub class: u8,
    pub max_packet0: u8,
    pub vid: u16,
    pub pid: u16,
    pub bcd_device: u16,
    pub i_manufacturer: u8,
    pub i_product: u8,
    pub i_serial: u8,
    pub num_configurations: u8,
}

impl DeviceDesc {
    pub fn parse(d: &[u8]) -> anyhow::Result<Self> {
        if d.len() < 18 || d[0] != 18 || d[1] != 1 {
            return Err(anyhow!("bad device descriptor: {d:02x?}"));
        }
        Ok(DeviceDesc {
            bcd_usb: u16::from_le_bytes([d[2], d[3]]),
            class: d[4],
            max_packet0: d[7],
            vid: u16::from_le_bytes([d[8], d[9]]),
            pid: u16::from_le_bytes([d[10], d[11]]),
            bcd_device: u16::from_le_bytes([d[12], d[13]]),
            i_manufacturer: d[14],
            i_product: d[15],
            i_serial: d[16],
            num_configurations: d[17],
        })
    }
}

/// An interface parsed out of a configuration descriptor.
#[derive(Debug, Clone)]
pub struct ParsedInterface {
    pub number: u8,
    pub alt: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    /// (address, transfer type, max packet size)
    pub endpoints: Vec<(u8, u8, u16)>,
}

pub fn parse_interfaces(config: &[u8]) -> Vec<ParsedInterface> {
    let mut out: Vec<ParsedInterface> = Vec::new();
    let mut off = 0;
    while off + 2 <= config.len() {
        let len = config[off] as usize;
        if len < 2 || off + len > config.len() {
            break;
        }
        let d = &config[off..off + len];
        match d[1] {
            4 if len >= 9 => out.push(ParsedInterface {
                number: d[2],
                alt: d[3],
                class: d[5],
                subclass: d[6],
                protocol: d[7],
                endpoints: vec![],
            }),
            5 if len >= 7 => {
                if let Some(i) = out.last_mut() {
                    i.endpoints
                        .push((d[2], d[3] & 3, u16::from_le_bytes([d[4], d[5]]) & 0x7ff));
                }
            }
            _ => {}
        }
        off += len;
    }
    out
}
