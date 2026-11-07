use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;
use tracing::info;
use usbshare_server::device::Backend;
use usbshare_server::mock::{MockBackend, MockKind};
use usbshare_server::Server;

/// Share USB devices over the network (USB/IP compatible).
#[derive(Parser)]
#[command(name = "usbshare-server", version)]
struct Cli {
    /// Address to listen on.
    #[arg(short, long, default_value = "0.0.0.0:3240", env = "USBSHARE_LISTEN")]
    listen: SocketAddr,

    /// Add a simulated device: loopback, hid, storage or storage:<MiB>. Repeatable.
    #[arg(
        short,
        long = "mock",
        value_name = "KIND",
        env = "USBSHARE_MOCK",
        value_delimiter = ','
    )]
    mock: Vec<MockKind>,

    /// Do not share physical USB devices (only mock devices).
    #[arg(long)]
    no_usb: bool,

    /// Only share these physical devices: bus id (1-1.2) or vid:pid / vid:* in hex. Repeatable.
    #[arg(long, value_name = "SELECTOR")]
    allow: Vec<String>,

    /// Never share these physical devices (same syntax as --allow). Repeatable.
    #[arg(long, value_name = "SELECTOR")]
    deny: Vec<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print the devices that would be shared and exit.
    List,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();

    let mut backends: Vec<Arc<dyn Backend>> = Vec::new();
    if !cli.mock.is_empty() {
        backends.push(Arc::new(MockBackend::new(&cli.mock)));
    }
    if !cli.no_usb {
        #[cfg(target_os = "linux")]
        backends.push(Arc::new(usbshare_server::usbfs::UsbfsBackend::new(
            usbshare_server::usbfs::Filter {
                allow: cli.allow.clone(),
                deny: cli.deny.clone(),
            },
        )));
    }
    let server = Server::new(backends);

    if let Some(Command::List) = cli.command {
        let devs = server.devices().await;
        if devs.is_empty() {
            println!("no shareable devices");
        }
        for (b, d) in devs {
            let d = d.device;
            println!(
                "{:<10} {:04x}:{:04x}  {:<6} {}",
                d.busid,
                d.id_vendor,
                d.id_product,
                b.name(),
                d.speed
            );
        }
        return Ok(());
    }

    let listener = TcpListener::bind(cli.listen)
        .await
        .with_context(|| format!("binding {}", cli.listen))?;
    for (b, d) in server.devices().await {
        let d = d.device;
        info!(
            "sharing {} {:04x}:{:04x} ({})",
            d.busid,
            d.id_vendor,
            d.id_product,
            b.name()
        );
    }
    tokio::select! {
        r = server.serve(listener) => r,
        _ = tokio::signal::ctrl_c() => { info!("shutting down"); Ok(()) }
    }
}
