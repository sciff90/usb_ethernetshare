use anyhow::bail;
use clap::{Parser, Subcommand};
use usbip_proto::ExportedDevice;
use usbshare_client::{host, remote, selftest};

/// Use USB devices shared over the network (USB/IP compatible).
#[derive(Parser)]
#[command(name = "usbshare", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List devices shared by a server.
    List {
        /// Server address: host or host:port (default port 3240).
        server: String,
    },
    /// Attach remote device(s) so they appear as local USB devices (Linux, needs vhci-hcd and root).
    Attach {
        server: String,
        /// Bus id (e.g. 1-1.2), vid:pid in hex, or "all".
        device: String,
    },
    /// Detach a device attached with `attach`.
    Detach {
        /// vhci port number (see `usbshare port`) or "all".
        port: String,
    },
    /// Show devices currently attached through vhci.
    Port,
    /// Print descriptors of a remote device (user space; no kernel support needed).
    Info { server: String, device: String },
    /// Exercise remote devices end to end from user space (no kernel support needed).
    Test {
        server: String,
        /// Bus id, vid:pid or "all" (default).
        #[arg(default_value = "all")]
        device: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    match Cli::parse().command {
        Command::List { server } => list(&server).await,
        Command::Attach { server, device } => attach(&server, &device).await,
        Command::Detach { port } => detach(&port),
        Command::Port => ports(),
        Command::Info { server, device } => info(&server, &device).await,
        Command::Test { server, device } => test(&server, &device).await,
    }
}

fn class_name(class: u8) -> &'static str {
    match class {
        0x01 => "audio",
        0x02 => "comm",
        0x03 => "HID",
        0x07 => "printer",
        0x08 => "mass storage",
        0x09 => "hub",
        0x0a => "CDC data",
        0x0e => "video",
        0xe0 => "wireless",
        0xef => "misc",
        0xff => "vendor",
        _ => "other",
    }
}

async fn list(server: &str) -> anyhow::Result<()> {
    let devs = remote::list(server).await?;
    if devs.is_empty() {
        println!("{server}: no devices available");
    }
    for d in &devs {
        let dev = &d.device;
        let classes: Vec<&str> = d.interfaces.iter().map(|i| class_name(i.class)).collect();
        println!(
            "{:<10} {:04x}:{:04x}  {:<18} [{}]",
            dev.busid,
            dev.id_vendor,
            dev.id_product,
            dev.speed.to_string(),
            classes.join(", ")
        );
    }
    Ok(())
}

async fn resolve(server: &str, selector: &str) -> anyhow::Result<Vec<ExportedDevice>> {
    let devs = remote::list(server).await?;
    let sel: Vec<ExportedDevice> = remote::select(&devs, selector).into_iter().cloned().collect();
    if sel.is_empty() {
        bail!("no available device matches '{selector}' on {server} (in-use devices are not listed)");
    }
    Ok(sel)
}

#[cfg(target_os = "linux")]
async fn attach(server: &str, selector: &str) -> anyhow::Result<()> {
    use usbshare_client::vhci::{save_record, Vhci};
    let vhci = Vhci::open()?;
    for d in resolve(server, selector).await? {
        let busid = &d.device.busid;
        let (stream, desc) = remote::import(server, busid).await?;
        let sock = stream.into_std()?;
        sock.set_nonblocking(false)?;
        let port = vhci.attach(&sock, desc.devid(), desc.speed)?;
        // The kernel holds its own reference to the socket; ours can be closed.
        drop(sock);
        save_record(port, server, busid);
        println!(
            "attached {server}/{busid} ({:04x}:{:04x}) on vhci port {port}",
            desc.id_vendor, desc.id_product
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn detach(port: &str) -> anyhow::Result<()> {
    use usbshare_client::vhci::Vhci;
    let vhci = Vhci::open()?;
    let ports: Vec<u32> = if port == "all" {
        vhci.ports()?
            .into_iter()
            .filter(|p| !p.is_free())
            .map(|p| p.port)
            .collect()
    } else {
        vec![anyhow::Context::context(
            port.parse::<u32>(),
            "port must be a number or 'all'",
        )?]
    };
    for p in ports {
        vhci.detach(p)?;
        println!("detached port {p}");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn ports() -> anyhow::Result<()> {
    use usbshare_client::vhci::{load_record, Vhci};
    let used: Vec<_> = Vhci::open()?
        .ports()?
        .into_iter()
        .filter(|p| !p.is_free())
        .collect();
    if used.is_empty() {
        println!("no devices attached");
    }
    for p in used {
        let remote = load_record(p.port)
            .map(|(s, b)| format!("{s}/{b}"))
            .unwrap_or_else(|| "?".into());
        println!(
            "port {:<3} {:<18} local {:<8} remote {}  status {}",
            p.port,
            p.speed.to_string(),
            p.local_busid,
            remote,
            p.status
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn attach(_: &str, _: &str) -> anyhow::Result<()> {
    bail!("attaching devices is only supported on Linux so far")
}
#[cfg(not(target_os = "linux"))]
fn detach(_: &str) -> anyhow::Result<()> {
    bail!("only supported on Linux so far")
}
#[cfg(not(target_os = "linux"))]
fn ports() -> anyhow::Result<()> {
    bail!("only supported on Linux so far")
}

async fn info(server: &str, selector: &str) -> anyhow::Result<()> {
    for d in resolve(server, selector).await? {
        let (stream, desc) = remote::import(server, &d.device.busid).await?;
        let h = host::UsbIpHost::new(stream, desc.devid());
        let dd = h.device_descriptor().await?;
        println!(
            "{}  {:04x}:{:04x}  USB {:x}.{:02x}  {}",
            desc.busid,
            dd.vid,
            dd.pid,
            dd.bcd_usb >> 8,
            dd.bcd_usb & 0xff,
            desc.speed
        );
        println!(
            "  manufacturer: {}",
            h.string(dd.i_manufacturer).await?.unwrap_or_default()
        );
        println!(
            "  product:      {}",
            h.string(dd.i_product).await?.unwrap_or_default()
        );
        println!(
            "  serial:       {}",
            h.string(dd.i_serial).await?.unwrap_or_default()
        );
        let cfg = h.config_descriptor().await?;
        for i in host::parse_interfaces(&cfg) {
            println!(
                "  interface {} alt {}: class {:02x} ({}) sub {:02x} proto {:02x}",
                i.number,
                i.alt,
                i.class,
                class_name(i.class),
                i.subclass,
                i.protocol
            );
            for (addr, ty, mps) in i.endpoints {
                let ty = ["control", "isochronous", "bulk", "interrupt"][ty as usize];
                let dir = if addr & 0x80 != 0 { "IN " } else { "OUT" };
                println!("    endpoint {addr:#04x} {dir} {ty:<11} max packet {mps}");
            }
        }
    }
    Ok(())
}

async fn test(server: &str, selector: &str) -> anyhow::Result<()> {
    let (mut passed, mut failed) = (0, 0);
    for d in resolve(server, selector).await? {
        let r = selftest::test_device(server, &d).await?;
        passed += r.passed;
        failed += r.failed;
    }
    println!("\n{passed} passed, {failed} failed");
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}
