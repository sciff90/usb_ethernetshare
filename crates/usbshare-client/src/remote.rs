//! Operation-phase requests to a server.

use anyhow::{bail, Context};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use usbip_proto::{self as proto, DeviceDescriptor, ExportedDevice, USBIP_PORT};

/// Normalise "host" / "host:port" / "[v6]:port" into something `connect` accepts.
pub fn server_addr(s: &str) -> String {
    if s.parse::<std::net::SocketAddr>().is_ok() {
        return s.to_string();
    }
    if s.parse::<std::net::Ipv6Addr>().is_ok() {
        return format!("[{s}]:{USBIP_PORT}");
    }
    match s.rsplit_once(':') {
        Some((_, p)) if p.parse::<u16>().is_ok() => s.to_string(),
        _ => format!("{s}:{USBIP_PORT}"),
    }
}

pub async fn connect(server: &str) -> anyhow::Result<TcpStream> {
    let addr = server_addr(server);
    let s = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("connecting to {addr}"))?;
    s.set_nodelay(true)?;
    Ok(s)
}

/// List the devices a server exports.
pub async fn list(server: &str) -> anyhow::Result<Vec<ExportedDevice>> {
    let mut s = connect(server).await?;
    s.write_all(&proto::encode_req_devlist()).await?;
    Ok(proto::read_rep_devlist(&mut s).await?)
}

/// Import (claim) a device. On success the returned stream is in the URB phase.
pub async fn import(server: &str, busid: &str) -> anyhow::Result<(TcpStream, DeviceDescriptor)> {
    let mut s = connect(server).await?;
    s.write_all(&proto::encode_req_import(busid)).await?;
    match proto::read_rep_import(&mut s).await? {
        Ok(d) => {
            if d.busid != busid {
                bail!("server returned device {} instead of {busid}", d.busid);
            }
            Ok((s, d))
        }
        Err(status) => bail!("server refused import of {busid}: {}", status_text(status)),
    }
}

pub fn status_text(status: u32) -> &'static str {
    match status {
        proto::ST_NA => "device not available",
        proto::ST_DEV_BUSY => "device is in use by another client",
        proto::ST_DEV_ERR => "server could not open the device",
        proto::ST_NODEV => "no such device",
        _ => "error",
    }
}

/// Resolve a user-supplied selector (bus id, `vid:pid`, or `all`) against a device list.
pub fn select<'a>(devs: &'a [ExportedDevice], selector: &str) -> Vec<&'a ExportedDevice> {
    if selector == "all" {
        return devs.iter().collect();
    }
    if let Some(d) = devs.iter().find(|d| d.device.busid == selector) {
        return vec![d];
    }
    if let Some((v, p)) = selector.split_once(':') {
        if let (Ok(v), Ok(p)) = (u16::from_str_radix(v, 16), u16::from_str_radix(p, 16)) {
            return devs
                .iter()
                .filter(|d| d.device.id_vendor == v && d.device.id_product == p)
                .collect();
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::server_addr;

    #[test]
    fn addresses() {
        assert_eq!(server_addr("pi4"), "pi4:3240");
        assert_eq!(server_addr("pi4:4000"), "pi4:4000");
        assert_eq!(server_addr("10.0.0.1"), "10.0.0.1:3240");
        assert_eq!(server_addr("::1"), "[::1]:3240");
        assert_eq!(server_addr("[::1]:99"), "[::1]:99");
    }
}
