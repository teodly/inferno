#!/bin/bash
# Regression test for teodly/inferno#8 (see test_status_before_prepare.c):
# querying the PCM before it is prepared must not abort the process.
# Needs the release plugin built (cargo build --release -p alsa_pcm_inferno)
# and libasound headers; no clock or network peer is required.
set -e
cd "$(dirname "$0")"
LIB="$(realpath ../target/release/libasound_module_pcm_inferno.so)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
cat > "$TMP/asoundrc" <<CONF
pcm_type.inferno { lib "$LIB" }
pcm.inferno_status_test { type inferno NAME "status-test" SAMPLE_RATE 48000 RX_CHANNELS 2 TX_CHANNELS 2 ALT_PORT 16000 }
CONF
cc -o "$TMP/status_test" test_status_before_prepare.c -lasound
ALSA_CONFIG_PATH="/usr/share/alsa/alsa.conf:$TMP/asoundrc" RUST_LOG=error timeout 30 "$TMP/status_test"
echo "PASS: no abort before prepare"
