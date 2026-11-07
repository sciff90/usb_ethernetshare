#!/bin/sh
# Create kernel-level dummy USB devices on the *server host* so the real
# (usbfs) backend can be tested without physical hardware.
#
# Uses dummy_hcd (a software USB host + device controller pair) and the USB
# gadget configfs interface to create one composite device (1d6b:0104) with:
#   * a mass-storage function backed by a 64 MiB image file
#   * a CDC-ACM serial function (server side: /dev/ttyGS0)
#
# Needs root and a kernel with CONFIG_USB_DUMMY_HCD and CONFIG_USB_CONFIGFS
# (stock Ubuntu/Debian/Fedora kernels have both as modules).
#
#   sudo scripts/dummy-gadget.sh up
#   usbshare-server --allow 1d6b:0104     # share only the gadget
#   sudo scripts/dummy-gadget.sh down
set -eu

G=/sys/kernel/config/usb_gadget/usbshare
IMG=${USBSHARE_GADGET_IMG:-/var/tmp/usbshare-gadget.img}

up() {
  modprobe libcomposite
  modprobe dummy_hcd
  mountpoint -q /sys/kernel/config || mount -t configfs none /sys/kernel/config
  [ -d "$G" ] && { echo "gadget already exists ($G)"; exit 0; }

  mkdir -p "$G"
  echo 0x1d6b > "$G/idVendor"   # Linux Foundation
  echo 0x0104 > "$G/idProduct"  # Multifunction Composite Gadget
  echo 0x0100 > "$G/bcdDevice"
  echo 0x0200 > "$G/bcdUSB"
  mkdir -p "$G/strings/0x409"
  echo "USBSHAREGADGET1" > "$G/strings/0x409/serialnumber"
  echo "usbshare"        > "$G/strings/0x409/manufacturer"
  echo "usbshare dummy gadget" > "$G/strings/0x409/product"

  mkdir -p "$G/configs/c.1/strings/0x409"
  echo "mass storage + serial" > "$G/configs/c.1/strings/0x409/configuration"
  echo 250 > "$G/configs/c.1/MaxPower"

  [ -f "$IMG" ] || truncate -s 64M "$IMG"
  mkdir -p "$G/functions/mass_storage.0"
  echo 1 > "$G/functions/mass_storage.0/lun.0/removable"
  echo "$IMG" > "$G/functions/mass_storage.0/lun.0/file"
  mkdir -p "$G/functions/acm.0"

  ln -s "$G/functions/mass_storage.0" "$G/configs/c.1/"
  ln -s "$G/functions/acm.0" "$G/configs/c.1/"

  UDC=$(ls /sys/class/udc | grep dummy_udc | head -1)
  [ -n "$UDC" ] || { echo "no dummy_udc found"; exit 1; }
  echo "$UDC" > "$G/UDC"
  sleep 1
  echo "gadget bound to $UDC:"
  lsusb -d 1d6b:0104 2>/dev/null || true
}

down() {
  [ -d "$G" ] || { echo "no gadget"; exit 0; }
  echo "" > "$G/UDC" 2>/dev/null || true
  rm -f "$G/configs/c.1/mass_storage.0" "$G/configs/c.1/acm.0"
  rmdir "$G/configs/c.1/strings/0x409" "$G/configs/c.1" 2>/dev/null || true
  rmdir "$G/functions/mass_storage.0" "$G/functions/acm.0" 2>/dev/null || true
  rmdir "$G/strings/0x409" "$G" 2>/dev/null || true
  echo "gadget removed"
}

case "${1:-}" in
  up) up ;;
  down) down ;;
  *) echo "usage: $0 up|down"; exit 2 ;;
esac
