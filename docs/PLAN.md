# Open-source USB-over-network sharing in Rust: implementation and test plan

Working name: **`usbshare`**. This is a placeholder. Choose a final name that does not use "VirtualHere" or anything that sounds like it.

## 1. Goals

The goal is to match VirtualHere's core features in an open-source, cross-platform Rust codebase:

| Feature | Target phase |
|---|---|
| Server shares the USB devices plugged into it | P1 |
| Client attaches a remote device so it shows up as a local USB device | P1 (Linux), P3 (Windows), P5 (macOS) |
| Control, bulk and interrupt transfers | P1 |
| Isochronous transfers (webcams, audio) | P4 |
| Hotplug: devices appear and disappear live on both sides | P2 |
| Finding servers automatically on the LAN (mDNS/Bonjour) | P2 |
| Auto-use (attach a device automatically when it shows up), nicknames, auto-reconnect | P2 |
| A client daemon controlled from a CLI over IPC, like VirtualHere's client service | P2 |
| TLS encryption, authentication, per-device access rules | P3 |
| Compression; reverse connections (the server dials out, for NAT) | P4 |
| A GUI | Later; not in scope now |

## 2. Main architecture decision: use the USB/IP protocol for the data path

The hardest part of this project is not the networking. It is making a remote device appear as a real local USB device on the client. That requires a **virtual USB host controller driver** in each client operating system's kernel. Writing and signing kernel drivers ourselves, especially for Windows, would take most of the project's time.

**Decision:** the data path uses the **USB/IP wire protocol**, and our own additions are layered around it. This means we can use existing, already-signed client drivers:

| OS | Virtual host controller | How we drive it |
|---|---|---|
| Linux | `vhci-hcd`, built into the kernel (from the usbip project) | Write `port sockfd devid speed` to `/sys/devices/platform/vhci_hcd.0/attach` |
| Windows | `usbip-win2`, an open-source, signed UDE driver | Its device IOCTL interface |
| macOS | No driver exists today. Research `IOUSBHostControllerInterface` (macOS 15+, needs an Apple entitlement) | P5, research spike |

**How we add TLS and other features on top of the kernel's plain TCP socket:** the client daemon creates a local `socketpair`. It gives one end to the kernel's `vhci-hcd` and relays the other end through our own transport (TLS, compression, reconnect logic). The kernel sees normal USB/IP traffic; everything on the network is wrapped by us.

**Benefits:**
- Our server works with the stock `usbip attach` tool. Our client works with a stock `usbipd` server. Each of these doubles as a test oracle (a known-correct reference to check against).
- We do not have to write kernel drivers for Linux or Windows.

**On the server side**, the server is a normal user-space program. It accesses USB devices through the [`nusb`](https://crates.io/crates/nusb) crate, which is pure Rust and supports Linux usbfs, Windows WinUSB and macOS IOKit. `rusb`/libusb is a fallback for isochronous transfers if nusb's support for them is still incomplete. So the server should run on Linux, Windows and macOS. Linux, including the Raspberry Pi, is the main target.

```
 ┌──────────── Server host ────────────┐          ┌──────────── Client host ─────────────┐
 │ USB device                          │          │                      apps / drivers   │
 │   │ nusb (usbfs/WinUSB/IOKit)       │          │                           │           │
 │ ┌─▼──────────────┐ ┌─────────────┐  │  TLS/TCP │ ┌──────────────┐  ┌───────▼───────┐   │
 │ │ UsbBackend     │◄┤ usbshare-   ├──┼──────────┼─┤ usbshare-    ├─►│ vhci-hcd /    │   │
 │ │ (real | mock)  │ │ server      │  │  mDNS    │ │ client daemon│  │ usbip-win2    │   │
 │ └────────────────┘ └─────────────┘  │          │ └──────▲───────┘  └───────────────┘   │
 └─────────────────────────────────────┘          │   IPC  │ (unix socket / named pipe)    │
                                                  │ ┌──────┴───────┐                      │
                                                  │ │ usbshare CLI │                      │
                                                  │ └──────────────┘                      │
                                                  └───────────────────────────────────────┘
```

## 3. Cargo workspace layout

```
usbshare/
├── Cargo.toml                 # workspace
├── crates/
│   ├── usbip-proto/           # USB/IP message encode/decode. No I/O, no_std-friendly, fuzzed
│   ├── usbshare-proto/        # our control protocol: handshake, version, auth, device events, nicknames
│   ├── usbshare-transport/    # TCP + rustls TLS, framing, optional zstd/lz4, keepalive, reconnect
│   ├── usbshare-discovery/    # mDNS advertise/browse (mdns-sd crate), _usbshare._tcp
│   ├── usbshare-backend/      # trait UsbBackend + NusbBackend + MockBackend (simulated devices)
│   ├── usbshare-server/       # lib: device registry, claim/release, URB executor, unlink, ACLs
│   ├── usbshare-vhci/         # trait VirtualHostController + linux / windows / macos implementations
│   ├── usbshare-client/       # lib: server sessions, attach manager, auto-use rules, IPC API
│   └── usbshare-cli/          # binaries: `usbshare-server`, `usbshare` (client daemon + CLI)
├── tests/                     # cross-crate integration and end-to-end tests
├── fuzz/                      # cargo-fuzz targets
└── xtask/                     # CI helpers: load vhci, set up dummy_hcd gadgets, packaging
```

Main dependencies: `tokio`, `bytes`, `thiserror`/`anyhow`, `tracing`, `clap`, `serde` + `toml` (config), `rustls` + `rcgen`, `mdns-sd`, `nusb`, `windows-sys` (vhci IOCTLs), `nix` (Linux sysfs and socketpair), `interprocess` (cross-platform IPC).

### Core traits

```rust
// usbshare-backend
pub trait UsbBackend: Send + Sync {
    fn list(&self) -> Vec<DeviceInfo>;                       // bus id, VID:PID, class, speed, strings
    fn hotplug(&self) -> BoxStream<'static, HotplugEvent>;
    async fn open(&self, id: &BusId) -> Result<Box<dyn OpenDevice>>;
}
pub trait OpenDevice: Send {
    async fn submit(&self, urb: Urb) -> Result<UrbCompletion>; // control/bulk/int/iso
    fn cancel(&self, seqnum: u32);                             // CMD_UNLINK
    async fn reset(&self) -> Result<()>;
    async fn set_configuration(&self, cfg: u8) -> Result<()>;  // intercepted standard requests
    async fn set_interface(&self, iface: u8, alt: u8) -> Result<()>;
}

// usbshare-vhci
pub trait VirtualHostController {
    fn free_port(&self, speed: Speed) -> Result<Port>;
    fn attach(&self, port: Port, sock: OwnedSocket, devid: u32, speed: Speed) -> Result<()>;
    fn detach(&self, port: Port) -> Result<()>;
    fn ports(&self) -> Result<Vec<PortStatus>>;
}
```

The `MockBackend` is the key to testing. It provides software-simulated devices with no hardware: a HID keyboard, a CDC-ACM serial loopback, a mass-storage device backed by an image file, and a "zero" device for transfer stress tests.

## 4. Protocol

**Data path (unchanged USB/IP):**
- `OP_REQ_DEVLIST` / `OP_REP_DEVLIST`: list the shared devices.
- `OP_REQ_IMPORT` / `OP_REP_IMPORT`: claim one device.
- `USBIP_CMD_SUBMIT` / `USBIP_RET_SUBMIT`: send a transfer request (URB) and get its result. Isochronous transfers include a descriptor for each packet.
- `USBIP_CMD_UNLINK` / `USBIP_RET_UNLINK`: cancel a transfer that is in progress.

**Control channel (ours):** one long-lived connection per client and server pair. It carries length-prefixed messages encoded with `postcard`, and is versioned from day one:
- `Hello{version, features}`, then authentication (`Auth{token | password-SCRAM}`, or a TLS client certificate).
- `ListDevices`, `Subscribe` (streams hotplug events and "in use by X" status), `SetNickname`, `Use{busid}`, `StopUsing`, `ServerInfo`.
- `Use` returns a one-time ticket. The data connection presents that ticket and then switches to plain USB/IP framing. This lets the server enforce access rules and in-use status, which stock USB/IP cannot.
- **Compatibility mode:** the server also accepts a bare USB/IP connection on port 3240, if enabled in config, so stock `usbip` clients work.

**Server-side details that are easy to get wrong:**
- Intercept `SET_CONFIGURATION`, `SET_INTERFACE` and `CLEAR_FEATURE(ENDPOINT_HALT)` and map them to the proper nusb calls. Do not pass them through as raw control transfers.
- Handle port reset.
- Keep a table of in-flight URBs per device, keyed by seqnum, so that `UNLINK` can cancel the right transfer.
- Keep each endpoint's transfers in order while running different endpoints concurrently.
- Limit the number of queued URBs to provide backpressure.
- On Linux, detach the kernel driver when claiming a device. Restore it on release.

## 5. CLI design

```bash
# Server
usbshare-server                       # foreground, shares everything permitted by config
usbshare-server --config /etc/usbshare/server.toml --listen 0.0.0.0:7575
usbshare-server list                  # local devices and their share status
usbshare-server install-service       # systemd / Windows service / launchd

# Client (the daemon runs in the background; the CLI talks to it over IPC)
usbshare daemon                       # or installed as a service
usbshare servers                      # discovered and manually added servers
usbshare add-server 192.168.1.20:7575
usbshare list                         # tree: server → devices (nickname, VID:PID, in-use-by)
usbshare use   pi4/1-1.2              # attach a device
usbshare stop  pi4/1-1.2
usbshare auto-use add --vid 046d --pid c52b    # rules: by device, by port, or everything on a server
usbshare nickname pi4/1-1.2 "Logitech receiver"
usbshare status --json                # all commands also support --json for scripting
```

Config files are TOML. Device selectors use the forms `server/busid`, `vid:pid[:serial]` or a nickname.

## 6. Phases and milestones

### P0: Foundations (about 1 week)
- Set up the workspace, CI matrix (Linux x86_64/aarch64, Windows, macOS), `clippy -D warnings`, `rustfmt`, `cargo-deny`.
- Implement `usbip-proto` with round-trip and property tests, and a fuzz target.
- **Exit criterion:** the protocol crate decodes captured Wireshark/usbip traces byte-for-byte.

### P1: Minimum viable product, Linux on both ends (about 3–4 weeks)
- `NusbBackend` with listing, open/claim, and control, bulk and interrupt transfers, plus unlink.
- `MockBackend` with the simulated devices listed in section 3.
- Server: TCP listener and USB/IP compatibility mode.
- Client: Linux `vhci` backend through sysfs, with a direct TCP socket (no relay yet).
- CLI: `usbshare-server`, `usbshare list/use/stop`, with no daemon yet.
- **Exit criteria:**
  - A USB flash drive and a keyboard attached through `usbshare use` work normally.
  - Our server works with the stock `usbip attach` tool.
  - Our client works with a stock `usbipd` server.

### P2: Making it usable (about 3 weeks)
- Client daemon with an IPC API. The CLI becomes a thin IPC client.
- Socketpair relay, so the data path goes through our transport.
- Control channel, hotplug events, in-use status, nicknames (stored on the server).
- mDNS discovery, auto-use rules, auto-reconnect with re-attach after the network drops.
- Install as a system service on each OS.
- **Exit criterion:** pull the network cable for 10 seconds and plug it back in, and the device comes back by itself. Plugging a device into the server makes it appear on the client automatically when an auto-use rule matches it.

### P3: Security and Windows client (about 3–4 weeks)
- TLS with rustls: either a self-signed server certificate with trust-on-first-use pinning, or certificates issued by a private certificate authority. Optional client certificates.
- Password or token authentication. Per-device access rules (which users can use which devices). Rate limiting failed authentication attempts.
- Windows `VirtualHostController` using the usbip-win2 driver. The installer bundles the driver or checks that it is present.
- Windows and macOS server through nusb. Document the WinUSB driver binding (Zadig/libwdi) that a Windows server needs.
- **Exit criteria:**
  - A Windows client can use a device shared from a Linux server, over TLS.
  - A security review is done of every input that comes from the network.

### P4: Isochronous transfers, performance and NAT (about 3 weeks)
- Isochronous transfers end to end: webcams and USB audio. Add the rusb fallback if nusb lacks support.
- Optional compression (zstd/lz4), negotiated per connection. Improve the transfer pipeline (batch writes, `TCP_NODELAY`, buffer reuse).
- Reverse connection mode, where the server dials out to the client.
- **Exit criteria:**
  - A USB webcam streams at 720p30 through `usbshare`.
  - Bulk transfers reach at least 80% of gigabit LAN speed.

### P5: macOS client (research, open-ended)
- Prototype a virtual host controller built on `IOUSBHostControllerInterface`, and find out what the entitlement requires.
- Until that works, macOS supports the server role and the CLI only, with the client limited to controlling remote attachments.

## 7. Test strategy

### 7.1 Unit tests (every commit, every OS)
- `usbip-proto`: golden byte vectors taken from real traces; `proptest` round-trip tests (`decode(encode(x)) == x`); rejection of bad lengths and unknown commands.
- `usbshare-proto`: version negotiation, unknown or extra fields, authentication state machine.
- Server URB executor run against the `MockBackend`: ordering, unlink racing with completion, backpressure, a device disappearing while transfers are in flight, and standard-request interception.
- Client attach manager: auto-use rule matching, reconnect backoff state machine. Time is controlled with `tokio::time::pause`.

### 7.2 Fuzzing (nightly CI, `cargo-fuzz`)
- Targets: the USB/IP decoder, the control-protocol decoder, and the server's handling of a full byte stream against the `MockBackend`.
- Goals: no panics, no unbounded memory allocation (reject oversized transfer buffers before allocating), no infinite loops.

### 7.3 In-process integration tests (every commit, every OS)
- Run the server with the `MockBackend` and the client library in one process, connected through a loopback socket, using a `FakeVhci`. The `FakeVhci` acts as the host, sending real USB/IP URBs that enumerate the device and do I/O.
- Scenarios:
  - Enumerate a mock device and read its descriptors.
  - Mass-storage round trip: write and read blocks, then compare checksums.
  - HID interrupt stream.
  - CDC loopback.
  - Hotplug while a device is attached.
  - Two clients trying to use one device (the second is refused).
  - Access rules deny a device.
  - TLS: a pinned certificate mismatch is rejected.
  - Wrong password is rejected.
- Network fault injection with a TCP proxy written in Rust (toxiproxy-style): added latency, bandwidth caps, dropped connections, half-open connections. Check that reconnect works and that nothing leaks.

### 7.4 Linux kernel end-to-end tests (CI on an Ubuntu VM with `linux-modules-extra`)
No physical USB hardware is needed for these tests:
- **Real devices without hardware:** the Linux `dummy_hcd` driver plus USB gadget drivers configured through configfs (`g_zero`, `g_mass_storage`, `g_hid`, `g_serial`, `g_audio`/UVC). These create real USB devices on the server side, which nusb then opens. This tests the real backend.
- **Real virtual host controller:** load `vhci-hcd` and attach through our client. Then check:
  - `lsusb` shows the device.
  - `mount` plus a checksum confirms mass-storage data.
  - The kernel's `testusb` tool (usbtest, run against `g_zero`) covers every transfer type and the unlink and cancel paths. This is the strongest conformance test available.
- **Interop matrix:** 4 combinations, our server or `usbipd` against our client or `usbip attach`.
- Run these tests in QEMU through `xtask` so that kernel-module tests run the same way locally and in CI.

### 7.5 Windows end-to-end tests (self-hosted runner or VM, nightly)
- Install usbip-win2 in test-signing mode. Attach a mock-backed device from a Linux server. Check that it shows up in Device Manager through PowerShell `Get-PnpDevice`. Run a mass-storage checksum.

### 7.6 Hardware-in-the-loop (nightly or before releases)
- A Raspberry Pi 4 server with a fixed set of devices: flash drive, keyboard and mouse, FTDI serial adapter, webcam, USB audio, a microcontroller programmer (for example ST-Link), and a USB hub. Use a programmable USB power switch to automate plug and unplug.
- Scripted checks for each device class. Soak test: 24 hours of attaching and detaching repeatedly plus continuous I/O, with no leaked file descriptors or memory (tracked with heaptrack).

### 7.7 Performance (tracked over time)
- Benchmarks with `criterion` for encoding and decoding and the relay.
- End-to-end throughput with `dd` against mass storage. HID latency measured in microseconds as the round-trip time of a mock interrupt transfer.
- CI fails if a result is more than 10% worse than the stored baseline.

### 7.8 Security
- Run `cargo-audit` and `cargo-deny` on every pull request.
- Every input from the network has a size limit. No `unsafe` code outside the `vhci` and Windows FFI modules. Those modules get a dedicated review.
- External audit of the authentication and TLS design before 1.0.

## 8. Risks

| Risk | Mitigation |
|---|---|
| Windows kernel drivers must be signed | Use the signed usbip-win2 driver and do not write our own |
| No macOS client driver exists | Treat macOS as a research phase. Ship server and CLI support for macOS first |
| nusb's isochronous support is incomplete | Keep rusb/libusb as a fallback behind the `UsbBackend` trait |
| Windows servers need WinUSB bound to each device | Document the setup, and possibly automate it with libwdi later |
| Isochronous timing over Wi-Fi or a WAN | Add buffering and document that isochronous use is best effort |
| Kernel usbip quirks across kernel versions | Run the CI interop matrix on several kernels (LTS releases plus latest) |
| Trademark or "clone" issues | Use an original name and branding. Implement only the published USB/IP protocol, and copy no VirtualHere code or protocol |

## 9. License
Apache-2.0 or MIT (dual license, the usual choice for Rust). Linux vhci is a kernel interface and usbip-win2 is GPL-3. We talk to both only through their syscall/IOCTL interfaces, which keeps our code license-independent. Bundling the usbip-win2 installer needs a check of its terms.

## 10. First concrete steps
1. Run `cargo new --lib` to create the workspace and crates. Set up CI.
2. Implement `usbip-proto` plus tests, using pcap traces captured from `usbip attach` against `usbipd`.
3. Implement `MockBackend` with the HID keyboard device, and `FakeVhci`. The first in-process end-to-end test enumerates the mock keyboard.
4. Add `NusbBackend` and the Linux `vhci` backend, then do the first real device attach on Linux.
