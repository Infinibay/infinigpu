#!/usr/bin/env bash
# infiniPixel v0 headless round-trip (no browser): start the demo (NVENC H.264 →
# owned framing → WebSocket), connect a Node client that speaks the infiniPixel
# protocol, collect N access units, and prove with ffmpeg that the streamed bytes are
# a valid, decodable H.264 stream of the right dimensions. Also writes a viewable PNG.
#
# This validates encode + own-protocol framing + WebSocket transport + a real client
# parsing the header — everything the browser client also does, minus the display.
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-8091}"
W="${W:-960}"; H="${H:-540}"; FPS="${FPS:-30}"; N="${N:-60}"
command -v ffmpeg >/dev/null || { echo "!! ffmpeg required"; exit 1; }
command -v node   >/dev/null || { echo "!! node (>=21, global WebSocket) required"; exit 1; }

cargo build -q -p infinigpu-pixel
DEMO=target/debug/infinigpu-pixel-demo

WORK="$(mktemp -d /tmp/infinipixel-XXXXXX)"
trap 'kill ${DEMOPID:-0} 2>/dev/null || true; rm -rf "$WORK"' EXIT

# ---- start the streamer ----
RUST_LOG=warn "$DEMO" --width "$W" --height "$H" --fps "$FPS" --port "$PORT" >"$WORK/demo.log" 2>&1 &
DEMOPID=$!
sleep 1.5
kill -0 "$DEMOPID" 2>/dev/null || { echo "!! demo failed to start:"; cat "$WORK/demo.log"; exit 1; }

# ---- Node client speaking the infiniPixel protocol ----
cat >"$WORK/client.mjs" <<'JS'
import fs from 'node:fs';
const [port, nframes, out] = [process.argv[2], parseInt(process.argv[3]), process.argv[4]];
const MAGIC = 0x49504958;
const chunks = []; let got = 0, keyframes = 0, w = 0, h = 0;
const ws = new WebSocket(`ws://127.0.0.1:${port}`);
ws.binaryType = 'arraybuffer';
const done = (code, msg) => { if (msg) console.error(msg); try { ws.close(); } catch {} process.exit(code); };
ws.onerror = (e) => done(4, 'ws error: ' + (e.message || e));
setTimeout(() => done(5, `timeout: only got ${got}/${nframes} frames`), 20000);
ws.onmessage = (ev) => {
  const dv = new DataView(ev.data), u8 = new Uint8Array(ev.data);
  if (dv.getUint32(0, true) !== MAGIC) return done(3, 'bad magic');
  const flags = u8[5], plen = dv.getUint32(24, true);
  w = dv.getUint16(12, true); h = dv.getUint16(14, true);
  if (flags & 1) keyframes++;
  chunks.push(Buffer.from(u8.subarray(32, 32 + plen)));
  if (++got >= nframes) {
    fs.writeFileSync(out, Buffer.concat(chunks));
    console.log(`OK frames=${got} keyframes=${keyframes} dims=${w}x${h} bytes=${Buffer.concat(chunks).length}`);
    done(0);
  }
};
JS

echo ">> connecting infiniPixel client (collecting $N access units)…"
CLIENT_OUT="$(node "$WORK/client.mjs" "$PORT" "$N" "$WORK/stream.h264")" || {
    echo "FAIL: client error: $CLIENT_OUT"; cat "$WORK/demo.log"; exit 1; }
echo "   $CLIENT_OUT"
CLIENT_KF="$(echo "$CLIENT_OUT" | grep -oE 'keyframes=[0-9]+' | cut -d= -f2)"
CLIENT_DIMS="$(echo "$CLIENT_OUT" | grep -oE 'dims=[0-9]+x[0-9]+' | cut -d= -f2)"

# ---- prove the collected stream is valid, decodable H.264 ----
DIMS="$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0 "$WORK/stream.h264" 2>/dev/null | tr ',' 'x')"
NF="$(ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames -of csv=p=0 "$WORK/stream.h264" 2>/dev/null || echo 0)"
ffmpeg -hide_banner -loglevel error -f h264 -i "$WORK/stream.h264" -vf "select=eq(n\,20)" -frames:v 1 -y /tmp/infinipixel-frame.png 2>/dev/null || true

echo
echo ">> ffprobe: dims=$DIMS decoded_frames=$NF"
echo ">> demo log:"; grep -E 'infiniPixel|NVENC|libx264' "$WORK/demo.log" | head -3 | sed 's/^/     /'

if [[ "$DIMS" == "${W}x${H}" && "${CLIENT_KF:-0}" -ge 1 && "${NF:-0}" -ge 1 && "$CLIENT_DIMS" == "${W}x${H}" ]]; then
    echo
    echo "PASS: infiniPixel streamed valid H.264 over its own protocol/WebSocket."
    echo "      - $N access units received, keyframes=$CLIENT_KF, header dims $CLIENT_DIMS;"
    echo "      - ffmpeg decoded $NF frames at $DIMS from the collected stream."
    echo "      Viewable decoded frame: /tmp/infinipixel-frame.png"
else
    echo "FAIL: dims=$DIMS (want ${W}x${H}) client_dims=$CLIENT_DIMS kf=$CLIENT_KF decoded=$NF"
    cat "$WORK/demo.log"; exit 1
fi
