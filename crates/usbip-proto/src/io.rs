//! Async helpers for reading USB/IP messages from a stream.

use crate::*;
use tokio::io::{AsyncRead, AsyncReadExt};

/// A request received by a server in the operation phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpRequest {
    DevList,
    Import { busid: String },
}

pub async fn read_op_header<R: AsyncRead + Unpin>(r: &mut R) -> Result<OpHeader> {
    let mut b = [0u8; OP_HEADER_LEN];
    r.read_exact(&mut b).await?;
    let h = OpHeader::decode(&b);
    if h.version >> 8 != USBIP_VERSION >> 8 {
        return Err(Error::Version(h.version));
    }
    Ok(h)
}

/// Server side: read an operation-phase request.
pub async fn read_op_request<R: AsyncRead + Unpin>(r: &mut R) -> Result<OpRequest> {
    let h = read_op_header(r).await?;
    match h.code {
        OP_REQ_DEVLIST => Ok(OpRequest::DevList),
        OP_REQ_IMPORT => {
            let mut b = [0u8; BUSID_LEN];
            r.read_exact(&mut b).await?;
            let end = b.iter().position(|&c| c == 0).unwrap_or(BUSID_LEN);
            Ok(OpRequest::Import {
                busid: String::from_utf8_lossy(&b[..end]).into_owned(),
            })
        }
        other => Err(Error::UnexpectedOp(other)),
    }
}

/// Client side: read an `OP_REP_DEVLIST`.
pub async fn read_rep_devlist<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<ExportedDevice>> {
    let h = read_op_header(r).await?;
    if h.code != OP_REP_DEVLIST {
        return Err(Error::UnexpectedOp(h.code));
    }
    let n = r.read_u32().await? as usize;
    if n > MAX_DEVICES {
        return Err(Error::TooLarge {
            what: "device count",
            len: n,
        });
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let mut b = vec![0u8; USB_DEVICE_LEN];
        r.read_exact(&mut b).await?;
        let device = DeviceDescriptor::decode(&b)?;
        let mut ib = vec![0u8; device.num_interfaces as usize * USB_INTERFACE_LEN];
        r.read_exact(&mut ib).await?;
        out.push(ExportedDevice {
            device,
            interfaces: decode_interfaces(&ib),
        });
    }
    Ok(out)
}

/// Client side: read an `OP_REP_IMPORT`. Returns `Err(status)` inside `Ok`
/// when the server refused the import.
pub async fn read_rep_import<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<std::result::Result<DeviceDescriptor, u32>> {
    let h = read_op_header(r).await?;
    if h.code != OP_REP_IMPORT {
        return Err(Error::UnexpectedOp(h.code));
    }
    if h.status != ST_OK {
        return Ok(Err(h.status));
    }
    let mut b = vec![0u8; USB_DEVICE_LEN];
    r.read_exact(&mut b).await?;
    Ok(Ok(DeviceDescriptor::decode(&b)?))
}

/// Read one URB-phase message. See [`UrbMessage::parse_header`] for `in_dir_for`.
pub async fn read_urb<R: AsyncRead + Unpin>(
    r: &mut R,
    in_dir_for: &mut (dyn FnMut(u32) -> Option<Direction> + Send),
) -> Result<UrbMessage> {
    let mut hdr = [0u8; URB_HEADER_LEN];
    r.read_exact(&mut hdr).await?;
    let pending = UrbMessage::parse_header(&hdr, in_dir_for)?;
    let mut payload = vec![0u8; pending.payload_len()];
    r.read_exact(&mut payload).await?;
    pending.finish(payload)
}
