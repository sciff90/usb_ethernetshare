//! Read-only web dashboard: which devices are shared, which are in use and by whom.
//!
//! * `GET /`            – HTML dashboard (polls the API every 2 s)
//! * `GET /api/status`  – JSON snapshot

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::SystemTime;

use axum::extract::State;
use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tokio::net::TcpListener;
use tracing::info;

use crate::Server;

const INDEX_HTML: &str = include_str!("web/index.html");

#[derive(Clone)]
struct AppState {
    server: Arc<Server>,
    usbip_listen: SocketAddr,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub server: ServerInfo,
    pub devices: Vec<DeviceStatus>,
}

#[derive(Debug, Serialize)]
pub struct ServerInfo {
    pub hostname: String,
    pub version: &'static str,
    /// Address the USB/IP service listens on.
    pub usbip_listen: String,
    pub uptime_secs: u64,
    /// Server clock, so the page can compute durations without clock skew.
    pub now_unix: u64,
}

#[derive(Debug, Serialize)]
pub struct DeviceStatus {
    pub busid: String,
    pub vid: String,
    pub pid: String,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub speed: String,
    pub backend: &'static str,
    pub interfaces: Vec<&'static str>,
    pub in_use: bool,
    pub client: Option<ClientInfo>,
    pub last_client: Option<LastClientInfo>,
}

#[derive(Debug, Serialize)]
pub struct ClientInfo {
    pub ip: String,
    pub port: u16,
    pub since_unix: u64,
    pub bytes_to_client: u64,
    pub bytes_from_client: u64,
    pub transfers: u64,
}

#[derive(Debug, Serialize)]
pub struct LastClientInfo {
    pub ip: String,
    pub since_unix: u64,
    pub ended_unix: u64,
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Present IPv4-mapped IPv6 addresses (::ffff:a.b.c.d) as plain IPv4.
fn ip_string(addr: SocketAddr) -> String {
    match addr.ip() {
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(|v4| v4.to_string())
            .unwrap_or_else(|| v6.to_string()),
        v4 => v4.to_string(),
    }
}

pub fn class_name(class: u8) -> &'static str {
    match class {
        0x01 => "Audio",
        0x02 => "Communications",
        0x03 => "HID",
        0x05 => "Physical",
        0x06 => "Imaging",
        0x07 => "Printer",
        0x08 => "Mass storage",
        0x09 => "Hub",
        0x0a => "CDC data",
        0x0b => "Smart card",
        0x0d => "Security",
        0x0e => "Video",
        0x0f => "Healthcare",
        0x10 => "Audio/video",
        0xdc => "Diagnostic",
        0xe0 => "Wireless",
        0xef => "Miscellaneous",
        0xfe => "Application",
        0xff => "Vendor specific",
        _ => "Other",
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "usbshare".into())
}

/// Build a status snapshot.
pub async fn status(server: &Server, usbip_listen: SocketAddr) -> Status {
    let now = SystemTime::now();
    let mut devices: Vec<DeviceStatus> = server
        .device_entries()
        .await
        .into_iter()
        .map(|(backend, e)| {
            let d = &e.exported.device;
            let usage = server.usage(&d.busid);
            let mut interfaces: Vec<&'static str> = e
                .exported
                .interfaces
                .iter()
                .map(|i| class_name(i.class))
                .collect();
            interfaces.dedup();
            DeviceStatus {
                busid: d.busid.clone(),
                vid: format!("{:04x}", d.id_vendor),
                pid: format!("{:04x}", d.id_product),
                manufacturer: e.manufacturer.clone(),
                product: e.product.clone(),
                speed: d.speed.to_string(),
                backend: backend.name(),
                interfaces,
                in_use: usage.is_some(),
                client: usage.map(|u| ClientInfo {
                    ip: ip_string(u.peer),
                    port: u.peer.port(),
                    since_unix: unix(u.since),
                    bytes_to_client: u.bytes_to_client.load(Ordering::Relaxed),
                    bytes_from_client: u.bytes_from_client.load(Ordering::Relaxed),
                    transfers: u.transfers.load(Ordering::Relaxed),
                }),
                last_client: server.last_use(&d.busid).map(|l| LastClientInfo {
                    ip: ip_string(l.peer),
                    since_unix: unix(l.since),
                    ended_unix: unix(l.ended),
                }),
            }
        })
        .collect();
    devices.sort_by(|a, b| a.busid.cmp(&b.busid));
    Status {
        server: ServerInfo {
            hostname: hostname(),
            version: env!("CARGO_PKG_VERSION"),
            usbip_listen: usbip_listen.to_string(),
            uptime_secs: now
                .duration_since(server.started())
                .map(|d| d.as_secs())
                .unwrap_or(0),
            now_unix: unix(now),
        },
        devices,
    }
}

async fn api_status(State(st): State<AppState>) -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(status(&st.server, st.usbip_listen).await),
    )
}

async fn index() -> impl IntoResponse {
    (
        [
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src data:",
            ),
        ],
        Html(INDEX_HTML),
    )
}

pub fn router(server: Arc<Server>, usbip_listen: SocketAddr) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(api_status))
        .with_state(AppState { server, usbip_listen })
}

/// Serve the dashboard until the listener fails.
pub async fn serve(
    server: Arc<Server>,
    listener: TcpListener,
    usbip_listen: SocketAddr,
) -> anyhow::Result<()> {
    info!("web dashboard on http://{}", listener.local_addr()?);
    axum::serve(listener, router(server, usbip_listen)).await?;
    Ok(())
}
