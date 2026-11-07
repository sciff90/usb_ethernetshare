# Isochronous (webcam / audio) support: work in progress

Branch: `iso-support`. Not merged to `main` yet: it has never run against real hardware.

## Done

- `device.rs`: `UrbRequest.iso` (per-packet offset/length), `UrbResponse.iso` + `start_frame`
  (per-packet actual length/status), and `UsbDevice::enqueue`. Devices that support it queue a
  transfer *before returning*, so several transfers can be in flight per endpoint. Isochronous
  streams need this; the server used to run one transfer at a time per endpoint.
- `server.rs`: the endpoint worker uses `enqueue` when available, otherwise `submit` as before.
  `complete()` builds RET_SUBMIT with one iso descriptor per request packet (host offsets and
  lengths, device actual lengths and statuses), `start_frame` and `error_count`.
- `usbfs_raw.rs`: raw Linux usbfs ioctl engine on a *second* fd to the same device.
  It covers DISCONNECT_CLAIM, RELEASE+CONNECT, SETINTERFACE, CLEAR_HALT, CONTROL, async
  SUBMITURB/REAPURBNDELAY via tokio `AsyncFd` (the fd polls writable on completion), and DISCARDURB.
  Unit tests check the struct sizes and ioctl numbers against `<linux/usbdevice_fs.h>` (64-bit),
  and the iso buffer repacking:
  - USB/IP OUT data sits at the host's offsets; usbfs wants packets back to back.
  - USB/IP IN replies carry only the received bytes, compacted.
- `usbfs.rs`: interfaces with an isochronous endpoint in any alternate setting (UVC/UAC
  streaming) are claimed by the raw engine; all others stay on nusb. Control requests addressed
  to raw interfaces or their endpoints (SET_INTERFACE, UVC probe/commit, UAC sample rate,
  CLEAR_FEATURE halt) go through the raw fd, because usbfs requires the fd that claimed the
  interface. Bulk, interrupt and iso transfers on raw endpoints are queued with `enqueue`.

## Still to do

1. **Mock isochronous endpoints** for hardware-free tests. Planned: loopback device interface 1,
   alt 0 with no endpoints, alt 1 with iso OUT 0x03 and iso IN 0x83 (256 B, interval 1).
   `MockDevice` needs alternate-setting support (`InterfaceSpec` extra alternates,
   SET/GET_INTERFACE state). `mock/mod.rs` currently returns -EXDEV for iso (see `TODO(iso)`).
2. **Client `UsbIpHost::submit_iso` + selftest**:
   - SET_INTERFACE alt 1;
   - iso OUT 8x64 B, then a vendor request reads back the byte count;
   - iso IN 8x256 B, checking per-packet actual lengths and counters;
   - back to alt 0, where iso stalls.
3. **Real hardware**, on a server with a webcam and a USB headset. Then, on the client:
   - Webcam (UVC): `usbshare attach <server> <busid>`, then
     `ffmpeg -f v4l2 -input_format mjpeg -video_size 640x480 -i /dev/video0 -frames 60 out.mkv`.
     Prefer MJPEG: uncompressed YUYV needs ~18 MB/s, more than Wi-Fi gives.
   - Audio (UAC): `arecord -l` / `aplay -l`, then
     `arecord -D hw:<card>,0 -f S16_LE -r 48000 -c 2 -d 5 t.wav` and `aplay -D hw:<card>,0 t.wav`.
   - Watch the server log at `RUST_LOG=usbshare_server=debug` and the client's `dmesg`.
4. Known risks to check on hardware:
   - **Network jitter:** uvcvideo queues ~5 URBs (~20 ms); snd-usb-audio fewer. Jitter above
     that drops video frames or causes audio crackles, especially over Wi-Fi.
   - **Packet limit:** usbfs allows at most 128 iso packets per URB (client drivers use up to 48).
   - **Bandwidth errors:** high-bandwidth alternate settings can fail with -ENOSPC on the server's
     host controller.
