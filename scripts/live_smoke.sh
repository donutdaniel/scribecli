#!/usr/bin/env bash
set -euo pipefail

if [[ "${OSTYPE:-}" != darwin* ]]; then
  echo "live_smoke.sh is currently macOS-only" >&2
  exit 1
fi

SCRIBECLI_BIN="${SCRIBECLI_BIN:-./target/debug/scribecli}"
ARTIFACTS_DIR="${SCRIBECLI_TEST_ARTIFACTS_DIR:-$(mktemp -d /tmp/scribecli-live-smoke.XXXXXX)}"
RESULT_JSON="${ARTIFACTS_DIR}/record-result.json"

if [[ ! -x "${SCRIBECLI_BIN}" ]]; then
  echo "scribecli binary not found or not executable at ${SCRIBECLI_BIN}" >&2
  echo "build it first with: cargo build" >&2
  exit 1
fi

echo "Using binary: ${SCRIBECLI_BIN}"
echo "Artifacts dir: ${ARTIFACTS_DIR}"

"${SCRIBECLI_BIN}" --output json setup
"${SCRIBECLI_BIN}" --output json doctor
"${SCRIBECLI_BIN}" --output json devices list

(
  sleep 1
  if command -v say >/dev/null 2>&1; then
    say "scribecli live smoke test. Native system audio capture should transcribe this sentence."
  else
    echo "say is not available; skipping generated system audio" >&2
  fi
) &

"${SCRIBECLI_BIN}" --output json record \
  --input-mode single-device \
  --duration-seconds 8 \
  --artifacts-dir "${ARTIFACTS_DIR}" \
  --session-name live-smoke > "${RESULT_JSON}"

python3 - <<'PY' "${RESULT_JSON}"
import json
import pathlib
import sys

result_path = pathlib.Path(sys.argv[1])
result = json.loads(result_path.read_text())

assert result["status"] == "completed", result

transcript_path = pathlib.Path(result["transcript_path"])
event_log_path = pathlib.Path(result["event_log_path"])
audio_path = pathlib.Path(result["audio_path"])

assert transcript_path.exists(), transcript_path
assert event_log_path.exists(), event_log_path
assert audio_path.exists(), audio_path

print("Smoke test passed")
print(f"session_id: {result['session_id']}")
print(f"audio_path: {audio_path}")
print(f"transcript_path: {transcript_path}")
print(f"text: {result['text']}")
PY
