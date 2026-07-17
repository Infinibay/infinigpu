#!/usr/bin/env bash
# Native viewer headless round-trip: start the infiniPixel test-pattern streamer, then run
# the NATIVE client (infinigpu-viewer) in headless mode — it opens the WebSocket, parses
# the owned FrameHeader, decodes H.264 with openh264, and verifies real RGBA frames come
# out. Connects LATE (after a periodic IDR) to exercise the join-resync path. This runs on
# a box with no display; the windowed path (winit + Vulkan) needs a Wayland/Win32 session.
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-8090}"; W="${W:-800}"; H="${H:-450}"; FPS="${FPS:-30}"; N="${N:-40}"
command -v node >/dev/null || { echo "!! node (>=21, global WebSocket) not needed here; skipping"; }

cargo build -q -p infinigpu-pixel -p infinigpu-viewer
DEMO=target/debug/infinigpu-pixel-demo
VIEWER=target/debug/infinigpu-viewer

WORK="$(mktemp -d /tmp/viewer-headless-XXXXXX)"
trap 'kill ${DEMOPID:-0} 2>/dev/null || true; rm -rf "$WORK"' EXIT

RUST_LOG=warn "$DEMO" --width "$W" --height "$H" --fps "$FPS" --port "$PORT" >"$WORK/demo.log" 2>&1 &
DEMOPID=$!
sleep 4   # connect after the first periodic IDR → forces the stale-prime + gap resync path
kill -0 "$DEMOPID" 2>/dev/null || { echo "!! streamer failed:"; cat "$WORK/demo.log"; exit 1; }

echo ">> native viewer (headless), decoding $N frames from ws://127.0.0.1:$PORT …"
OUT="$(RUST_LOG=warn "$VIEWER" --headless --frames "$N" --url "ws://127.0.0.1:$PORT" --out "$WORK/last.ppm" 2>&1)"
ERRORS="$(echo "$OUT" | grep -c 'decode error' || true)"
echo "$OUT" | grep -E 'Decoded|OK —|stream ended after' || true

if ! echo "$OUT" | grep -q "OK —"; then echo "FAIL: viewer did not decode $N frames"; echo "$OUT" | tail; exit 1; fi
if [ "$ERRORS" -ne 0 ]; then echo "FAIL: $ERRORS decode errors (join-resync regression)"; exit 1; fi
if command -v file >/dev/null; then file "$WORK/last.ppm"; fi

echo "PASS: native client decoded valid H.264 frames from infiniPixel (0 decode errors, clean join-resync)."
