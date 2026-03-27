#!/usr/bin/env bash
set -euo pipefail

if [[ "${OSTYPE:-}" != darwin* ]]; then
  echo "start_stop_smoke.sh is currently macOS-only" >&2
  exit 1
fi

SCRIBECLI_BIN="${SCRIBECLI_BIN:-./target/debug/scribecli}"
WORK_DIR="${SCRIBECLI_SMOKE_DIR:-$(mktemp -d /tmp/scribecli-start-stop.XXXXXX)}"
START_JSON="${WORK_DIR}/start.json"
STOP_JSON="${WORK_DIR}/stop.json"
SESSION_NAME="start-stop-smoke-$(date +%s)"

if [[ ! -x "${SCRIBECLI_BIN}" ]]; then
  echo "scribecli binary not found or not executable at ${SCRIBECLI_BIN}" >&2
  echo "build it first with: cargo build" >&2
  exit 1
fi

echo "Using binary: ${SCRIBECLI_BIN}"
echo "Scratch dir: ${WORK_DIR}"

"${SCRIBECLI_BIN}" --output json setup
"${SCRIBECLI_BIN}" --output json doctor

"${SCRIBECLI_BIN}" --output json start \
  --input-mode single-device \
  --session-name "${SESSION_NAME}" > "${START_JSON}"

(
  sleep 1
  if command -v say >/dev/null 2>&1; then
    say "scribecli start stop smoke complete"
  else
    echo "say is not available; skipping generated system audio" >&2
  fi
) &

sleep 6
"${SCRIBECLI_BIN}" --output json stop > "${STOP_JSON}"

python3 - <<'PY' "${START_JSON}" "${STOP_JSON}"
import json
import pathlib
import sys

start_path = pathlib.Path(sys.argv[1])
stop_path = pathlib.Path(sys.argv[2])

start = json.loads(start_path.read_text())
stop = json.loads(stop_path.read_text())

assert start["status"] == "recording", start
assert stop["status"] == "completed", stop
assert stop["session_id"] == start["session_id"], (start, stop)

transcript_path = pathlib.Path(stop["transcript_path"])
audio_path = pathlib.Path(stop["audio_path"])
event_log_path = pathlib.Path(stop["event_log_path"])

assert transcript_path.exists(), transcript_path
assert audio_path.exists(), audio_path
assert event_log_path.exists(), event_log_path

print("Start/stop smoke test passed")
print(f"session_id: {stop['session_id']}")
print(f"audio_path: {audio_path}")
print(f"transcript_path: {transcript_path}")
print(f"text: {stop['text']}")
PY
