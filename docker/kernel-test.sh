#!/bin/sh
# Kernel end-to-end test: attaches the remote mock devices through vhci-hcd so
# they become real USB devices on this (the host's) kernel, then uses them
# through the normal Linux drivers.
#
# Needs: host kernel with vhci-hcd, a privileged container (writable /sys, /dev).
# Usage: kernel-test.sh <server>
set -eu
SERVER=${1:-server}

fail() { echo "FAIL: $*"; usbshare detach all >/dev/null 2>&1 || true; exit 1; }

if [ ! -e /sys/devices/platform/vhci_hcd.0/attach ]; then
  modprobe vhci-hcd 2>/dev/null || true
fi
[ -e /sys/devices/platform/vhci_hcd.0/attach ] || fail "vhci-hcd not loaded (run 'sudo modprobe vhci-hcd' on the host)"

i=0
until usbshare list "$SERVER" >/dev/null 2>&1; do
  i=$((i + 1)); [ $i -gt 60 ] && fail "server not reachable"
  sleep 0.5
done

echo "== attaching all devices from $SERVER"
usbshare detach all >/dev/null 2>&1 || true
usbshare attach "$SERVER" all
usbshare port

echo "== waiting for enumeration"
for id in 1209:0001 1209:0002 1209:0003; do
  i=0
  until lsusb -d "$id" >/dev/null 2>&1; do
    i=$((i + 1)); [ $i -gt 40 ] && fail "$id did not enumerate"
    sleep 0.25
  done
  echo "ok    lsusb shows $(lsusb -d "$id")"
done

echo "== mass storage"
DISK=""
i=0
while [ -z "$DISK" ]; do
  for d in /sys/block/sd*; do
    [ -e "$d" ] || continue
    if grep -q "RAM Disk" "$d/device/model" 2>/dev/null; then DISK=/dev/$(basename "$d"); fi
  done
  i=$((i + 1)); [ $i -gt 40 ] && fail "no block device appeared"
  [ -z "$DISK" ] && sleep 0.25
done
[ -b "$DISK" ] || mknod "$DISK" b "$(cat /sys/block/$(basename $DISK)/dev | tr ':' ' ')"
echo "ok    block device $DISK ($(cat /sys/block/$(basename $DISK)/size) sectors)"
head -c 4194304 /dev/urandom > /tmp/pattern
dd if=/tmp/pattern of="$DISK" bs=64k oflag=direct status=none || fail "write to $DISK"
dd if="$DISK" of=/tmp/readback bs=64k count=64 iflag=direct status=none || fail "read from $DISK"
cmp /tmp/pattern /tmp/readback || fail "read-back mismatch"
echo "ok    4 MiB written and verified with O_DIRECT"
mkfs.ext4 -q -F "$DISK" && mkdir -p /mnt/usb && mount "$DISK" /mnt/usb \
  && echo hello > /mnt/usb/hello.txt && sync && umount /mnt/usb \
  && echo "ok    ext4 filesystem created, mounted and written" || fail "filesystem test"

echo "== HID"
HID=$(grep -l "HID_ID=0003:00001209:00000002" /sys/class/hidraw/*/device/uevent 2>/dev/null | head -1 || true)
[ -n "$HID" ] && echo "ok    hidraw node $(basename $(dirname $(dirname $HID)))" || fail "no hidraw device"

echo "== detaching"
usbshare detach all
echo "all kernel tests passed"
