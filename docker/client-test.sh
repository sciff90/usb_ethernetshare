#!/bin/sh
# User-space end-to-end test: needs no kernel USB support and no privileges.
# Usage: client-test.sh <server>
set -eu
SERVER=${1:-server}

echo "== waiting for $SERVER"
i=0
until usbshare list "$SERVER" >/dev/null 2>&1; do
  i=$((i + 1)); [ $i -gt 60 ] && { echo "server not reachable"; exit 1; }
  sleep 0.5
done

echo "== usbshare list $SERVER"
usbshare list "$SERVER"

if command -v usbip >/dev/null 2>&1; then
  echo "== stock 'usbip list -r $SERVER' (interoperability with kernel USB/IP tools)"
  usbip list -r "$SERVER"
else
  echo "== stock usbip tool not installed (image built with INSTALL_TOOLS=0), skipping interop check"
fi

echo "== usbshare info $SERVER all"
usbshare info "$SERVER" all

echo "== usbshare test $SERVER"
usbshare test "$SERVER"
