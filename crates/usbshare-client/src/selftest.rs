//! `usbshare test`: exercise remote devices end to end from user space.
//!
//! Every device gets generic descriptor checks. The usbshare mock devices
//! (VID 0x1209) additionally get functional tests. Real mass-storage devices
//! only get *read-only* tests.

use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use usbip_proto::ExportedDevice;

use crate::host::{parse_interfaces, TransferError, UsbIpHost};
use crate::remote;

const MOCK_VID: u16 = 0x1209;
const T: Duration = Duration::from_secs(5);

pub struct Report {
    pub passed: usize,
    pub failed: usize,
}

struct Checker {
    report: Report,
}

impl Checker {
    fn record<T>(&mut self, name: &str, r: anyhow::Result<T>) -> Option<T> {
        match r {
            Ok(v) => {
                println!("  ok    {name}");
                self.report.passed += 1;
                Some(v)
            }
            Err(e) => {
                println!("  FAIL  {name}: {e:#}");
                self.report.failed += 1;
                None
            }
        }
    }
}

/// Run all applicable tests against one exported device.
pub async fn test_device(server: &str, dev: &ExportedDevice) -> anyhow::Result<Report> {
    let d = &dev.device;
    println!("{} {:04x}:{:04x}", d.busid, d.id_vendor, d.id_product);
    let mut c = Checker {
        report: Report { passed: 0, failed: 0 },
    };

    let Some((stream, imported)) = c.record("import", remote::import(server, &d.busid).await) else {
        return Ok(c.report);
    };
    let host = UsbIpHost::new(stream, imported.devid());

    let desc = c.record(
        "device descriptor",
        async {
            let dd = host.device_descriptor().await?;
            ensure!(
                dd.vid == d.id_vendor && dd.pid == d.id_product,
                "VID:PID mismatch with device list"
            );
            Ok(dd)
        }
        .await,
    );
    let config = c.record(
        "configuration descriptor",
        async {
            let cfg = host.config_descriptor().await?;
            let ifs = parse_interfaces(&cfg);
            ensure!(!ifs.is_empty(), "no interfaces");
            Ok(cfg)
        }
        .await,
    );
    if let Some(dd) = &desc {
        c.record(
            "string descriptors",
            async {
                let product = host.string(dd.i_product).await?;
                let mfr = host.string(dd.i_manufacturer).await?;
                println!(
                    "        {} / {}",
                    mfr.as_deref().unwrap_or("-"),
                    product.as_deref().unwrap_or("-")
                );
                Ok(())
            }
            .await,
        );
    }
    let cfg_value = config.as_ref().map(|c| c[5]).unwrap_or(1);
    c.record("SET_CONFIGURATION", host.set_configuration(cfg_value).await);

    let ifs = config.as_deref().map(parse_interfaces).unwrap_or_default();
    let is_mock = d.id_vendor == MOCK_VID;
    match (is_mock, d.id_product) {
        (true, 0x0001) => loopback_tests(&host, &mut c).await,
        (true, 0x0002) => hid_tests(&host, &mut c).await,
        _ => {}
    }
    if let Some(msc) = ifs
        .iter()
        .find(|i| i.class == 8 && i.subclass == 6 && i.protocol == 0x50)
    {
        let ep_in = msc
            .endpoints
            .iter()
            .find(|e| e.1 == 2 && e.0 & 0x80 != 0)
            .map(|e| e.0);
        let ep_out = msc
            .endpoints
            .iter()
            .find(|e| e.1 == 2 && e.0 & 0x80 == 0)
            .map(|e| e.0);
        if let (Some(ep_in), Some(ep_out)) = (ep_in, ep_out) {
            let bot = Bot {
                host: &host,
                ep_in,
                ep_out,
                tag: 1,
            };
            storage_tests(bot, &mut c, is_mock).await;
        }
    }
    Ok(c.report)
}

async fn loopback_tests(host: &UsbIpHost, c: &mut Checker) {
    for &size in &[1usize, 63, 512, 513, 4096, 65536, 1 << 20] {
        let data: Vec<u8> = (0..size).map(|i| (i * 7 + size) as u8).collect();
        c.record(
            &format!("loopback echo {size} bytes"),
            async {
                host.write(0x01, &data, T).await?;
                let mut got = Vec::new();
                while got.len() < size {
                    got.extend(host.read(0x81, 65536, T).await?);
                }
                ensure!(got == data, "data mismatch");
                Ok(())
            }
            .await,
        );
    }
    c.record(
        "unlink pending IN transfer",
        async {
            match host.read(0x81, 512, Duration::from_millis(300)).await {
                Err(TransferError::Timeout) => {}
                other => bail!("expected timeout+unlink, got {other:?}"),
            }
            // Device must still work afterwards.
            host.write(0x01, b"ping", T).await?;
            ensure!(
                host.read(0x81, 512, T).await? == b"ping",
                "echo after unlink failed"
            );
            Ok(())
        }
        .await,
    );
    c.record(
        "loopback throughput",
        async {
            let chunk = vec![0x5au8; 64 * 1024];
            let total = 32 * 1024 * 1024;
            let start = Instant::now();
            let mut moved = 0;
            while moved < total {
                host.write(0x01, &chunk, T).await?;
                let mut got = 0;
                while got < chunk.len() {
                    got += host.read(0x81, 65536, T).await?.len();
                }
                moved += chunk.len();
            }
            let secs = start.elapsed().as_secs_f64();
            println!(
                "        {:.1} MiB/s each way",
                (total as f64 / (1024.0 * 1024.0)) / secs
            );
            Ok(())
        }
        .await,
    );
}

async fn hid_tests(host: &UsbIpHost, c: &mut Checker) {
    c.record(
        "HID report descriptor",
        async {
            let rd = host.control_in(0x81, 6, 0x2200, 0, 256).await?;
            ensure!(
                rd.len() > 10 && rd[0] == 0x06,
                "unexpected report descriptor {rd:02x?}"
            );
            Ok(())
        }
        .await,
    );
    c.record(
        "HID SET_IDLE",
        host.control_out(0x21, 0x0a, 0, 0, &[]).await.map_err(Into::into),
    );
    c.record(
        "HID interrupt IN reports",
        async {
            let mut last = None;
            for _ in 0..3 {
                let r = host.read(0x81, 8, T).await?;
                ensure!(r.len() == 8, "report length {}", r.len());
                let n = u32::from_le_bytes(r[..4].try_into().unwrap());
                if let Some(l) = last {
                    ensure!(n > l, "counter did not increase ({l} -> {n})");
                }
                last = Some(n);
            }
            Ok(())
        }
        .await,
    );
    c.record(
        "HID SET_REPORT round trip",
        async {
            host.control_out(0x21, 0x09, 0x0200, 0, &[0xde, 0xad, 0xbe, 0xef, 0, 0, 0, 0])
                .await?;
            let r = host.read(0x81, 8, T).await?;
            ensure!(
                r[4..8] == [0xde, 0xad, 0xbe, 0xef],
                "output not reflected: {r:02x?}"
            );
            Ok(())
        }
        .await,
    );
    c.record(
        "HID interrupt OUT round trip",
        async {
            host.write(0x01, &[1, 2, 3, 4, 5, 6, 7, 8], T).await?;
            let r = host.read(0x81, 8, T).await?;
            ensure!(r[4..8] == [1, 2, 3, 4], "output not reflected: {r:02x?}");
            Ok(())
        }
        .await,
    );
}

/// Bulk-Only Transport helper.
struct Bot<'a> {
    host: &'a UsbIpHost,
    ep_in: u8,
    ep_out: u8,
    tag: u32,
}

struct BotResult {
    data: Vec<u8>,
    status: u8,
}

impl Bot<'_> {
    async fn cmd(&mut self, cb: &[u8], data_out: &[u8], len_in: u32) -> anyhow::Result<BotResult> {
        self.tag += 1;
        let dir_in = len_in > 0;
        let len = if dir_in { len_in } else { data_out.len() as u32 };
        let mut cbw = Vec::with_capacity(31);
        cbw.extend_from_slice(&0x4342_5355u32.to_le_bytes());
        cbw.extend_from_slice(&self.tag.to_le_bytes());
        cbw.extend_from_slice(&len.to_le_bytes());
        cbw.push(if dir_in { 0x80 } else { 0 });
        cbw.push(0); // LUN
        cbw.push(cb.len() as u8);
        let mut block = [0u8; 16];
        block[..cb.len()].copy_from_slice(cb);
        cbw.extend_from_slice(&block);
        self.host.write(self.ep_out, &cbw, T).await.context("CBW")?;
        let mut data = Vec::new();
        if dir_in {
            data = self.host.read(self.ep_in, len_in, T).await.context("data in")?;
        } else if !data_out.is_empty() {
            self.host
                .write(self.ep_out, data_out, T)
                .await
                .context("data out")?;
        }
        let csw = self.host.read(self.ep_in, 13, T).await.context("CSW")?;
        ensure!(
            csw.len() == 13 && csw[..4] == 0x5342_5355u32.to_le_bytes(),
            "bad CSW {csw:02x?}"
        );
        ensure!(
            u32::from_le_bytes(csw[4..8].try_into().unwrap()) == self.tag,
            "CSW tag mismatch"
        );
        Ok(BotResult {
            data,
            status: csw[12],
        })
    }
}

async fn storage_tests(mut bot: Bot<'_>, c: &mut Checker, writable: bool) {
    c.record(
        "SCSI INQUIRY",
        async {
            let r = bot.cmd(&[0x12, 0, 0, 0, 36, 0], &[], 36).await?;
            ensure!(
                r.status == 0 && r.data.len() >= 36,
                "status {} len {}",
                r.status,
                r.data.len()
            );
            println!(
                "        {} {}",
                String::from_utf8_lossy(&r.data[8..16]).trim(),
                String::from_utf8_lossy(&r.data[16..32]).trim()
            );
            Ok(())
        }
        .await,
    );
    c.record(
        "SCSI TEST UNIT READY",
        async {
            let r = bot.cmd(&[0; 6], &[], 0).await?;
            ensure!(r.status == 0, "status {}", r.status);
            Ok(())
        }
        .await,
    );
    let blocks = c.record(
        "SCSI READ CAPACITY",
        async {
            let r = bot.cmd(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], &[], 8).await?;
            ensure!(r.status == 0 && r.data.len() == 8, "status {}", r.status);
            let last = u32::from_be_bytes(r.data[..4].try_into().unwrap()) as u64;
            let bs = u32::from_be_bytes(r.data[4..8].try_into().unwrap());
            println!(
                "        {} blocks x {} bytes = {} MiB",
                last + 1,
                bs,
                (last + 1) * bs as u64 / 1048576
            );
            ensure!(bs == 512, "only 512-byte blocks are tested");
            Ok(last + 1)
        }
        .await,
    );
    let Some(blocks) = blocks else { return };
    let read10 = |lba: u32, n: u16| {
        let l = lba.to_be_bytes();
        let c = n.to_be_bytes();
        [0x28, 0, l[0], l[1], l[2], l[3], 0, c[0], c[1], 0]
    };
    c.record(
        "SCSI READ(10) block 0",
        async {
            let r = bot.cmd(&read10(0, 1), &[], 512).await?;
            ensure!(
                r.status == 0 && r.data.len() == 512,
                "status {} len {}",
                r.status,
                r.data.len()
            );
            Ok(())
        }
        .await,
    );
    if !writable {
        println!("        (real device: write tests skipped)");
        return;
    }
    c.record(
        "SCSI WRITE(10)/READ(10) verify 64 KiB",
        async {
            let lba = (blocks / 2) as u32;
            let pattern: Vec<u8> = (0..65536u32).map(|i| (i ^ (i >> 8) ^ lba) as u8).collect();
            let l = lba.to_be_bytes();
            let w = bot
                .cmd(&[0x2a, 0, l[0], l[1], l[2], l[3], 0, 0, 128, 0], &pattern, 0)
                .await?;
            ensure!(w.status == 0, "write status {}", w.status);
            let r = bot.cmd(&read10(lba, 128), &[], 65536).await?;
            ensure!(r.status == 0, "read status {}", r.status);
            ensure!(r.data == pattern, "read-back mismatch");
            Ok(())
        }
        .await,
    );
    c.record(
        "SCSI out-of-range read fails with sense",
        async {
            let r = bot.cmd(&read10(blocks as u32, 1), &[], 512).await?;
            ensure!(r.status == 1, "expected failed status, got {}", r.status);
            let s = bot.cmd(&[0x03, 0, 0, 0, 18, 0], &[], 18).await?;
            ensure!(
                s.data.len() >= 14 && s.data[2] == 5 && s.data[12] == 0x21,
                "sense {:02x?}",
                s.data
            );
            Ok(())
        }
        .await,
    );
}
