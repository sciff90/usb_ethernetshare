# usbshare

Share USB devices over the network. This is an open-source, cross-platform alternative to VirtualHere, written in Rust.

> Status: **phase 1, Linux server and client.** Device data travels over the standard
> [USB/IP protocol](https://docs.kernel.org/usb/usbip_protocol.html), so usbshare also works with the
> kernel's own `usbip` / `usbipd` tools. See [docs/PLAN.md](docs/PLAN.md) for the roadmap.

```
 server host                                      client host
 ┌──────────────────────────────┐   TCP 3240     ┌──────────────────────────────┐
 │ USB device ─ usbfs (nusb) ─┐ │   USB/IP       │ usbshare attach ──► vhci-hcd │──► lsusb, /dev/sdX,
 │ mock devices ──────────────┴─┼─► usbshare-  ◄─┼─ (or `usbshare test`:        │    /dev/hidraw, ...
 │                              │   server       │  user-space host, no kernel) │
 └──────────────────────────────┘                └──────────────────────────────┘
```

## Crates

| crate | what |
|---|---|
| `usbip-proto` | Encodes and decodes the USB/IP messages. No I/O. Size limits protect against hostile peers. |
| `usbshare-server` | The `usbshare-server` binary. It contains the URB engine (in-order handling per endpoint, unlink, exclusive claim), the real-device backend (Linux usbfs via `nusb`) and the simulated mock devices. |
| `usbshare-client` | The `usbshare` binary. It provides `list`, `attach`/`detach`/`port` (through Linux `vhci-hcd`), and `info`/`test`, which run a small user-space USB host and need no kernel support. |

## Build

```bash
cargo build --release
# target/release/usbshare-server  target/release/usbshare
```

## Usage

```bash
# Server: share every physical device except hubs. Add simulated devices with --mock.
sudo usbshare-server
sudo usbshare-server --allow 046d:c52b --allow 1-1.2     # only these devices
usbshare-server --no-usb --mock loopback,hid,storage:64   # simulated devices only, no root needed
usbshare-server list                                      # show what would be shared

# Client
usbshare list myserver                 # devices on the server
sudo modprobe vhci-hcd                 # once per boot
sudo usbshare attach myserver 1-1.2    # bus id, vid:pid, or "all"
usbshare port                          # attached devices
sudo usbshare detach all

# No kernel support needed:
usbshare info myserver all             # dump descriptors
usbshare test myserver                 # functional tests (see below)
```

## Simulated devices

`--mock` adds devices that exist only in software. Their VID is 1209, from pid.codes, and they use pid.codes' test PIDs.

| kind | id | on a Linux client after `attach` |
|---|---|---|
| `loopback` | 1209:0001 | Vendor-specific bulk device. Whatever is written to EP 0x01 can be read back from EP 0x81. |
| `hid` | 1209:0002 | Vendor-defined HID device that appears as `/dev/hidrawN`. It sends a counter report every 100 ms and echoes output reports. It is not a keyboard or mouse, so it never injects input. |
| `storage[:MiB]` | 1209:0003 | USB mass-storage device (Bulk-Only Transport, SCSI) backed by RAM, default 16 MiB. It appears as `/dev/sdX` and can be formatted and mounted. |

`usbshare test` runs these checks against every device:
- descriptors and SET_CONFIGURATION;
- loopback echo at sizes from 1 B to 1 MiB, unlink of a pending transfer, and throughput;
- HID reports, SET_REPORT and interrupt OUT;
- SCSI INQUIRY, READ CAPACITY, write/read verify, and the sense data returned for an out-of-range read.

On real mass-storage devices it runs read-only checks only.

## Testing with Docker

```bash
# 1. User-space test: works anywhere, with no USB hardware, kernel modules or privileges
docker compose up --build --abort-on-container-exit --exit-code-from client client

# 2. Kernel test: the client container attaches the simulated devices through the host's
#    vhci-hcd. They then show up in lsusb, as /dev/sdX (written, verified, ext4 mounted)
#    and as /dev/hidraw. Containers share the host kernel, so the module is loaded on the host.
sudo modprobe vhci-hcd
docker compose --profile kernel up --build --abort-on-container-exit --exit-code-from kernel-client kernel-client

# 3. Real-device backend without hardware: create a kernel dummy gadget (dummy_hcd + configfs)
#    on the host, then share it
sudo scripts/dummy-gadget.sh up
docker compose --profile hardware up --build server-usb    # or: sudo usbshare-server --allow 1d6b:0104
usbshare test localhost                                    # read-only checks on the gadget's disk
sudo scripts/dummy-gadget.sh down
```

`docker build --build-arg INSTALL_TOOLS=0` gives a minimal image that leaves out the stock `usbip`, `lsusb` and the disk tools.

## Unit and integration tests

```bash
cargo test
```

This runs the protocol unit tests (including a decoder fuzz loop), vhci sysfs parsing against a fake sysfs tree, and full in-process server and client tests. The in-process tests cover every mock-device check, exclusive claim and release on disconnect, and refusal of unknown devices.

## Known limitations (phase 1)

- **Isochronous transfers** (webcams, audio) are not supported yet. Such URBs fail with `-EXDEV`.
- The real-device backend keeps **one transfer in flight per endpoint**, which limits bulk throughput.
- `URB_ZERO_PACKET` is not forwarded to real devices yet.
- There is no authentication or encryption yet. Only run the server on trusted networks (planned for phase 3).
- The client daemon, mDNS discovery, auto-use and reconnect are planned for phase 2.

## License

MIT OR Apache-2.0
