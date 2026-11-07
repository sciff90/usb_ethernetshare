//! Preparing USB mass-storage devices before they are handed to a remote host.
//!
//! When the server takes a disk away from its own kernel, the kernel's storage
//! stack often "stops" it on the way out (like "safely remove"): the device then
//! reports *Medium not present* and the remote host sees a 0-byte disk.
//! [`wake_mass_storage`] sends SCSI START STOP UNIT (LoEj=1, Start=1) over the
//! Bulk-Only Transport to load and start the medium again.

use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::device::{UrbRequest, UrbResponse, UsbDevice};

const TIMEOUT: Duration = Duration::from_secs(5);
const CBW_SIGNATURE: u32 = 0x4342_5355; // "USBC"
const CSW_SIGNATURE: u32 = 0x5342_5355; // "USBS"
const TAG: u32 = 0x7573_6273; // "usbs"

/// A Bulk-Only Transport mass-storage interface found in a configuration descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BotInterface {
    pub number: u8,
    pub ep_in: u8,
    pub ep_out: u8,
}

/// Find BOT/SCSI interfaces (class 08, subclass 06, protocol 50), alternate setting 0.
pub fn find_bot_interfaces(config: &[u8]) -> Vec<BotInterface> {
    let mut out = Vec::new();
    let mut current: Option<(u8, Option<u8>, Option<u8>)> = None;
    let flush = |c: Option<(u8, Option<u8>, Option<u8>)>, out: &mut Vec<BotInterface>| {
        if let Some((number, Some(ep_in), Some(ep_out))) = c {
            out.push(BotInterface {
                number,
                ep_in,
                ep_out,
            });
        }
    };
    let mut off = 0;
    while off + 2 <= config.len() {
        let len = config[off] as usize;
        if len < 2 || off + len > config.len() {
            break;
        }
        let d = &config[off..off + len];
        match d[1] {
            4 if len >= 9 => {
                flush(current.take(), &mut out);
                if d[3] == 0 && (d[5], d[6], d[7]) == (0x08, 0x06, 0x50) {
                    current = Some((d[2], None, None));
                }
            }
            5 if len >= 7 => {
                if let Some((_, ep_in, ep_out)) = current.as_mut() {
                    if d[3] & 3 == 2 {
                        if d[2] & 0x80 != 0 {
                            ep_in.get_or_insert(d[2]);
                        } else {
                            ep_out.get_or_insert(d[2]);
                        }
                    }
                }
            }
            _ => {}
        }
        off += len;
    }
    flush(current, &mut out);
    out
}

async fn transfer(dev: &dyn UsbDevice, req: UrbRequest) -> Option<UrbResponse> {
    let cancel = CancellationToken::new();
    match tokio::time::timeout(TIMEOUT, dev.submit(req, &cancel)).await {
        Ok(r) if r.status == 0 => Some(r),
        Ok(r) => {
            debug!(status = r.status, "mass-storage wake transfer failed");
            None
        }
        Err(_) => {
            cancel.cancel();
            debug!("mass-storage wake transfer timed out");
            None
        }
    }
}

async fn clear_halt(dev: &dyn UsbDevice, ep: u8) {
    let setup = [0x02, 0x01, 0, 0, ep, 0, 0, 0]; // CLEAR_FEATURE(ENDPOINT_HALT)
    let req = UrbRequest {
        endpoint: 0x00,
        setup,
        data: vec![],
        length: 0,
        transfer_flags: 0,
        iso: vec![],
    };
    let _ = transfer(dev, req).await;
}

/// Run one data-less SCSI command over BOT. Returns the CSW status
/// (0 = passed, 1 = failed, 2 = phase error), or `None` on a transport error.
pub async fn bot_command(dev: &dyn UsbDevice, bot: BotInterface, lun: u8, cdb: &[u8]) -> Option<u8> {
    let mut cbw = Vec::with_capacity(31);
    cbw.extend_from_slice(&CBW_SIGNATURE.to_le_bytes());
    cbw.extend_from_slice(&TAG.to_le_bytes());
    cbw.extend_from_slice(&0u32.to_le_bytes()); // no data stage
    cbw.push(0); // direction: n/a
    cbw.push(lun);
    cbw.push(cdb.len() as u8);
    let mut block = [0u8; 16];
    block[..cdb.len()].copy_from_slice(cdb);
    cbw.extend_from_slice(&block);

    let out = UrbRequest {
        endpoint: bot.ep_out,
        setup: [0; 8],
        data: cbw,
        length: 0,
        transfer_flags: 0,
        iso: vec![],
    };
    if transfer(dev, out).await.is_none() {
        clear_halt(dev, bot.ep_out).await;
        return None;
    }
    let csw_req = UrbRequest {
        endpoint: bot.ep_in,
        setup: [0; 8],
        data: vec![],
        length: 13,
        transfer_flags: 0,
        iso: vec![],
    };
    let csw = match transfer(dev, csw_req.clone()).await {
        Some(r) => r,
        None => {
            // A stalled status stage is cleared and retried once (BOT spec 6.7.2).
            clear_halt(dev, bot.ep_in).await;
            transfer(dev, csw_req).await?
        }
    };
    let d = csw.data;
    if d.len() != 13 || d[..4] != CSW_SIGNATURE.to_le_bytes() || d[4..8] != TAG.to_le_bytes() {
        return None;
    }
    Some(d[12])
}

/// Load and start the medium of every BOT interface in `config`. Best effort:
/// failures are logged at debug level and otherwise ignored.
pub async fn wake_mass_storage(dev: &dyn UsbDevice, busid: &str, config: &[u8]) {
    for bot in find_bot_interfaces(config) {
        // START STOP UNIT, LoEj=1 Start=1: load the medium and spin it up.
        let status = bot_command(dev, bot, 0, &[0x1b, 0, 0, 0, 0x03, 0]).await;
        match status {
            Some(0) => info!(%busid, interface = bot.number, "mass storage: medium loaded and started"),
            other => {
                debug!(%busid, interface = bot.number, ?other, "mass storage: START STOP UNIT not accepted")
            }
        }
        // Clear any sense data left by the command so the remote host starts clean.
        let _ = bot_command(dev, bot, 0, &[0x00, 0, 0, 0, 0, 0]).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockDevice, StorageDevice};

    const TEST_UNIT_READY: [u8; 6] = [0; 6];
    const EJECT: [u8; 6] = [0x1b, 0, 0, 0, 0x02, 0];

    #[test]
    fn finds_bot_interface() {
        let dev = MockDevice::new(StorageDevice::new("S".into(), 1));
        let cfg = dev.spec().config_descriptor();
        assert_eq!(
            find_bot_interfaces(&cfg),
            vec![BotInterface {
                number: 0,
                ep_in: 0x81,
                ep_out: 0x02
            }]
        );
        let lb = MockDevice::new(crate::mock::LoopbackDevice::new("L".into()));
        assert!(find_bot_interfaces(&lb.spec().config_descriptor()).is_empty());
    }

    #[tokio::test]
    async fn wakes_ejected_disk() {
        let dev = MockDevice::new(StorageDevice::new("S".into(), 1));
        let cfg = dev.spec().config_descriptor();
        let bot = find_bot_interfaces(&cfg)[0];

        assert_eq!(bot_command(&dev, bot, 0, &TEST_UNIT_READY).await, Some(0));
        // Eject, as the server's kernel does when it lets go of the disk.
        assert_eq!(bot_command(&dev, bot, 0, &EJECT).await, Some(0));
        assert_eq!(bot_command(&dev, bot, 0, &TEST_UNIT_READY).await, Some(1));

        wake_mass_storage(&dev, "100-1", &cfg).await;
        assert_eq!(bot_command(&dev, bot, 0, &TEST_UNIT_READY).await, Some(0));
    }
}
