//! TCP server speaking the USB/IP protocol.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::Context;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use usbip_proto::{
    self as proto, iso_count, read_op_request, read_urb, CmdSubmit, Direction, ExportedDevice, IsoPacket,
    OpRequest, RetSubmit, RetUnlink, UrbMessage,
};

use crate::device::{Backend, DeviceEntry, UrbRequest, UsbDevice};

/// Live information about a client using a device.
#[derive(Debug)]
pub struct Usage {
    pub peer: SocketAddr,
    pub since: SystemTime,
    /// Bytes sent from the device to the client (IN data).
    pub bytes_to_client: AtomicU64,
    /// Bytes sent from the client to the device (OUT data).
    pub bytes_from_client: AtomicU64,
    /// Completed transfers.
    pub transfers: AtomicU64,
}

impl Usage {
    fn new(peer: SocketAddr) -> Self {
        Usage {
            peer,
            since: SystemTime::now(),
            bytes_to_client: AtomicU64::new(0),
            bytes_from_client: AtomicU64::new(0),
            transfers: AtomicU64::new(0),
        }
    }
}

/// The most recent finished session of a device.
#[derive(Debug, Clone, Copy)]
pub struct LastUse {
    pub peer: SocketAddr,
    pub since: SystemTime,
    pub ended: SystemTime,
}

/// The set of backends plus bookkeeping of which devices are in use.
pub struct Server {
    backends: Vec<Arc<dyn Backend>>,
    in_use: Mutex<HashMap<String, Arc<Usage>>>,
    last_use: Mutex<HashMap<String, LastUse>>,
    started: SystemTime,
}

impl Server {
    pub fn new(backends: Vec<Arc<dyn Backend>>) -> Arc<Server> {
        Arc::new(Server {
            backends,
            in_use: Mutex::new(HashMap::new()),
            last_use: Mutex::new(HashMap::new()),
            started: SystemTime::now(),
        })
    }

    pub fn started(&self) -> SystemTime {
        self.started
    }

    /// Current usage of `busid`, if a client has it attached.
    pub fn usage(&self, busid: &str) -> Option<Arc<Usage>> {
        self.in_use.lock().unwrap().get(busid).cloned()
    }

    /// The last finished session of `busid`, if any.
    pub fn last_use(&self, busid: &str) -> Option<LastUse> {
        self.last_use.lock().unwrap().get(busid).copied()
    }

    /// All devices from all backends, with names and the backend that owns each one.
    pub async fn device_entries(&self) -> Vec<(Arc<dyn Backend>, DeviceEntry)> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for b in &self.backends {
            match b.list().await {
                Ok(devs) => {
                    for d in devs {
                        let busid = &d.exported.device.busid;
                        if seen.insert(busid.clone()) {
                            out.push((b.clone(), d));
                        } else {
                            warn!(%busid, "duplicate bus id, ignoring");
                        }
                    }
                }
                Err(e) => warn!(backend = b.name(), "failed to list devices: {e:#}"),
            }
        }
        out
    }

    /// All devices from all backends, with the backend that owns each one.
    pub async fn devices(&self) -> Vec<(Arc<dyn Backend>, ExportedDevice)> {
        self.device_entries()
            .await
            .into_iter()
            .map(|(b, e)| (b, e.exported))
            .collect()
    }

    /// Which client (if any) currently uses `busid`.
    pub fn user_of(&self, busid: &str) -> Option<SocketAddr> {
        self.in_use.lock().unwrap().get(busid).map(|u| u.peer)
    }

    /// Accept connections forever.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> anyhow::Result<()> {
        info!("listening on {}", listener.local_addr()?);
        loop {
            let (stream, peer) = listener.accept().await?;
            let _ = stream.set_nodelay(true);
            let this = self.clone();
            // Run each connection in its own task and watch it from a second one, so a
            // panic is logged instead of silently dropping the connection.
            let conn = tokio::spawn(async move { this.handle(stream, peer).await });
            tokio::spawn(async move {
                match conn.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => warn!(%peer, "connection error: {e:#}"),
                    Err(e) if e.is_panic() => {
                        let msg = e.into_panic();
                        let msg = msg
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| msg.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".into());
                        error!(%peer, "connection handler panicked: {msg}");
                    }
                    Err(e) => warn!(%peer, "connection task failed: {e}"),
                }
            });
        }
    }

    async fn handle(self: Arc<Self>, mut stream: TcpStream, peer: SocketAddr) -> anyhow::Result<()> {
        let req = read_op_request(&mut stream).await.context("reading request")?;
        match req {
            OpRequest::DevList => {
                // Devices in use are not offered (same behaviour as the kernel usbipd).
                let devs: Vec<ExportedDevice> = self
                    .devices()
                    .await
                    .into_iter()
                    .map(|(_, d)| d)
                    .filter(|d| self.user_of(&d.device.busid).is_none())
                    .collect();
                info!(%peer, count = devs.len(), "device list requested");
                stream.write_all(&proto::encode_rep_devlist(&devs)).await?;
                Ok(())
            }
            OpRequest::Import { busid } => self.import(stream, peer, busid).await,
        }
    }

    async fn import(&self, mut stream: TcpStream, peer: SocketAddr, busid: String) -> anyhow::Result<()> {
        let found = self
            .devices()
            .await
            .into_iter()
            .find(|(_, d)| d.device.busid == busid);
        let Some((backend, exported)) = found else {
            warn!(%peer, %busid, "import of unknown device");
            stream
                .write_all(&proto::encode_rep_import(proto::ST_NODEV, None))
                .await?;
            return Ok(());
        };

        // Claim exclusively.
        let busy = {
            let mut in_use = self.in_use.lock().unwrap();
            match in_use.get(&busid) {
                Some(other) => Some(other.peer),
                None => {
                    in_use.insert(busid.clone(), Arc::new(Usage::new(peer)));
                    None
                }
            }
        };
        if let Some(other) = busy {
            warn!(%peer, %busid, %other, "device busy");
            stream
                .write_all(&proto::encode_rep_import(proto::ST_DEV_BUSY, None))
                .await?;
            return Ok(());
        }
        let _claim = ClaimGuard {
            server: self,
            busid: busid.clone(),
        };

        info!(%peer, %busid, backend = backend.name(), "import requested, opening device");
        let device = match backend.open(&busid).await {
            Ok(d) => d,
            Err(e) => {
                warn!(%peer, %busid, "failed to open device: {e:#}");
                stream
                    .write_all(&proto::encode_rep_import(proto::ST_DEV_ERR, None))
                    .await?;
                return Ok(());
            }
        };

        stream
            .write_all(&proto::encode_rep_import(proto::ST_OK, Some(&exported.device)))
            .await?;
        info!(%peer, %busid, vid = format!("{:04x}", exported.device.id_vendor),
              pid = format!("{:04x}", exported.device.id_product), "device attached");

        let usage = self.usage(&busid).expect("claimed above");
        let result = run_urb_session(stream, device.clone(), usage).await;
        device.release().await;
        info!(%peer, %busid, "device detached");
        result
    }
}

struct ClaimGuard<'a> {
    server: &'a Server,
    busid: String,
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        if let Some(u) = self.server.in_use.lock().unwrap().remove(&self.busid) {
            let last = LastUse {
                peer: u.peer,
                since: u.since,
                ended: SystemTime::now(),
            };
            self.server
                .last_use
                .lock()
                .unwrap()
                .insert(self.busid.clone(), last);
        }
    }
}

struct Job {
    cmd: CmdSubmit,
    token: CancellationToken,
}

/// Bookkeeping shared between the reader loop and endpoint workers.
///
/// Invariant: a seqnum is in `pending` until exactly one of
/// (a) its completion is sent as RET_SUBMIT, or (b) it is unlinked and a
/// RET_UNLINK is sent. The decision is made while holding the lock, and the
/// reply is queued while still holding it, so replies are never reordered.
struct Session {
    pending: Mutex<HashMap<u32, CancellationToken>>,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    usage: Arc<Usage>,
}

/// Process URBs for an imported device until the connection closes.
pub async fn run_urb_session(
    stream: TcpStream,
    device: Arc<dyn UsbDevice>,
    usage: Arc<Usage>,
) -> anyhow::Result<()> {
    let (mut rd, mut wr) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let session = Arc::new(Session {
        pending: Mutex::new(HashMap::new()),
        tx,
        usage,
    });

    let writer = tokio::spawn(async move {
        while let Some(buf) = rx.recv().await {
            if wr.write_all(&buf).await.is_err() {
                break;
            }
        }
    });

    // One worker per endpoint keeps transfers on an endpoint in order
    // (required e.g. for mass storage CBW -> data -> CSW sequencing).
    let mut workers: HashMap<u8, mpsc::UnboundedSender<Job>> = HashMap::new();
    let shutdown = CancellationToken::new();

    let result: anyhow::Result<()> = loop {
        let msg = match read_urb(&mut rd, &mut |_| None).await {
            Ok(m) => m,
            Err(proto::Error::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break Ok(()),
            Err(e) => break Err(e.into()),
        };
        match msg {
            UrbMessage::CmdSubmit(cmd) => {
                let ep_num = (cmd.ep & 0x0f) as u8;
                // Control endpoint is bidirectional: one queue for both directions.
                let key = if ep_num == 0 {
                    0
                } else {
                    ep_num | if cmd.direction == Direction::In { 0x80 } else { 0 }
                };
                let token = shutdown.child_token();
                session.pending.lock().unwrap().insert(cmd.seqnum, token.clone());
                let worker = workers
                    .entry(key)
                    .or_insert_with(|| spawn_endpoint_worker(device.clone(), session.clone()));
                let _ = worker.send(Job { cmd, token });
            }
            UrbMessage::CmdUnlink(u) => {
                let mut pending = session.pending.lock().unwrap();
                let status = match pending.remove(&u.unlink_seqnum) {
                    Some(tok) => {
                        tok.cancel();
                        -proto::errno::ECONNRESET
                    }
                    None => 0, // already completed
                };
                debug!(seq = u.unlink_seqnum, status, "unlink");
                let _ = session.tx.send(
                    UrbMessage::RetUnlink(RetUnlink {
                        seqnum: u.seqnum,
                        status,
                    })
                    .encode(),
                );
            }
            other => break Err(anyhow::anyhow!("unexpected message from client: {other:?}")),
        }
    };

    shutdown.cancel();
    drop(workers);
    drop(session);
    let _ = writer.await;
    result
}

fn spawn_endpoint_worker(device: Arc<dyn UsbDevice>, session: Arc<Session>) -> mpsc::UnboundedSender<Job> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
    tokio::spawn(async move {
        while let Some(Job { cmd, token }) = rx.recv().await {
            if token.is_cancelled() {
                continue;
            }
            let n_iso = iso_count(cmd.number_of_packets);
            // Isochronous transfers are not supported yet: report every packet as failed
            // (the host still needs one descriptor per packet).
            let iso_reply: Vec<IsoPacket> = cmd
                .iso_packets
                .iter()
                .map(|p| IsoPacket {
                    offset: p.offset,
                    length: p.length,
                    actual_length: 0,
                    status: -proto::errno::EXDEV,
                })
                .collect();
            let ep_addr = (cmd.ep as u8 & 0x0f) | if cmd.direction == Direction::In { 0x80 } else { 0 };
            let req = UrbRequest {
                endpoint: ep_addr,
                setup: cmd.setup,
                data: cmd.data,
                length: cmd.transfer_buffer_length,
                transfer_flags: cmd.transfer_flags,
                iso_packets: n_iso,
            };
            let resp = device.submit(req, &token).await;

            let mut pending = session.pending.lock().unwrap();
            if pending.remove(&cmd.seqnum).is_none() {
                continue; // unlinked meanwhile; RET_UNLINK already sent
            }
            let mut data = resp.data;
            let mut actual = resp.actual_length;
            if cmd.direction == Direction::In {
                data.truncate(cmd.transfer_buffer_length as usize);
                actual = data.len() as u32;
                session
                    .usage
                    .bytes_to_client
                    .fetch_add(actual as u64, Ordering::Relaxed);
            } else {
                data.clear();
                session
                    .usage
                    .bytes_from_client
                    .fetch_add(actual as u64, Ordering::Relaxed);
            }
            session.usage.transfers.fetch_add(1, Ordering::Relaxed);
            let ret = RetSubmit {
                seqnum: cmd.seqnum,
                status: resp.status,
                actual_length: actual,
                start_frame: 0,
                number_of_packets: if n_iso > 0 { cmd.number_of_packets } else { 0 },
                error_count: iso_reply.len() as i32,
                data,
                iso_packets: iso_reply,
            };
            let _ = session.tx.send(UrbMessage::RetSubmit(ret).encode());
        }
    });
    tx
}
