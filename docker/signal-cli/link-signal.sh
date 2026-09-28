#!/bin/sh
set -eu

/usr/local/bin/signal-cli --config /var/lib/signal-cli/data link -n pocket-agent 2>&1 |
while IFS= read -r line; do
  printf '%s\n' "$line"
  case "$line" in
    sgnl://linkdevice*)
      printf '\nScan this QR code in Signal: Settings -> Linked devices -> +\n\n'
      qrencode -t ANSIUTF8 "$line"
      ;;
  esac
done
