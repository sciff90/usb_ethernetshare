//! TCP server speaking the USB/IP protocol.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use usbip_proto::{
    self as proto, iso_count, read_op_request, read_urb, CmdSubmit, Direction, ExportedDevice, IsoPacket,
    OpRequest, RetSubmit, RetUnlink, UrbMessage,
};

use crate::device::{Backend, UrbRequest, UsbDevice};

/// The set of backends plus bookkeeping of which devices are in use.
pub struct Server {
    backends: Vec<Arc<dyn Backend>>,
    in_use: Mutex<HashMap<String, SocketAddr>>,
}

impl Server {
    pub fn new(backends: Vec<Arc<dyn Backend>>) -> Arc<Server> {
        Arc::new(Server {
            backends,
            in_use: Mutex::new(HashMap::new()),
        })
    }

    /// All devices from all backends, with the backend that owns each one.
    pub async fn devices(&self) -> Vec<(Arc<dyn Backend>, ExportedDevice)> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for b in &self.backends {
            match b.list().await {
                Ok(devs) => {
                    for d in devs {
                        if seen.insert(d.device.busid.clone()) {
                            out.push((b.clone(), d));
                        } else {
                            warn!(busid = %d.device.busid, "duplicate bus id, ignoring");
                        }
                    }
                }
                Err(e) => warn!(backend = b.name(), "failed to list devices: {e:#}"),
            }
        }
        out
    }

    /// Which client (if any) currently uses `busid`.
    pub fn user_of(&self, busid: &str) -> Option<SocketAddr> {
        self.in_use.lock().unwrap().get(busid).copied()
    }

    /// Accept connections forever.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> anyhow::Result<()> {
        info!("listening on {}", listener.local_addr()?);
        loop {
            let (stream, peer) = listener.accept().await?;
            let _ = stream.set_nodelay(true);
            let this = self.clone();
            tokio::spawn(async move {
                if let Err(e) = this.handle(stream, peer).await {
                    debug!(%peer, "connection ended: {e:#}");
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
            match in_use.get(&busid).copied() {
                Some(other) => Some(other),
                None => {
                    in_use.insert(busid.clone(), peer);
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

        let result = run_urb_session(stream, device.clone()).await;
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
        self.server.in_use.lock().unwrap().remove(&self.busid);
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
}

/// Process URBs for an imported device until the connection closes.
pub async fn run_urb_session(stream: TcpStream, device: Arc<dyn UsbDevice>) -> anyhow::Result<()> {
    let (mut rd, mut wr) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let session = Arc::new(Session {
        pending: Mutex::new(HashMap::new()),
        tx,
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
            } else {
                data.clear();
            }
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
