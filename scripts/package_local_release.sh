#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
OUTPUT_ROOT="${1:-${REPO_ROOT}/dist/local}"
PACKAGE_DIR="${OUTPUT_ROOT}/scribecli-local"
BIN_DIR="${PACKAGE_DIR}/bin"
LIBEXEC_DIR="${PACKAGE_DIR}/libexec/scribecli"

mkdir -p "${BIN_DIR}" "${LIBEXEC_DIR}"

cd "${REPO_ROOT}"
cargo build --release

cp "${REPO_ROOT}/target/release/scribecli" "${BIN_DIR}/scribecli"
codesign --force --sign - "${BIN_DIR}/scribecli" >/dev/null 2>&1 || true
"${SCRIPT_DIR}/build_bundled_whisper_cli.sh" "${LIBEXEC_DIR}/whisper-cli" >/dev/null
cp "${REPO_ROOT}/README.md" "${REPO_ROOT}/LICENSE" "${PACKAGE_DIR}/"

echo "Packaged local release at ${PACKAGE_DIR}"
echo "Binary: ${BIN_DIR}/scribecli"
echo "Bundled whisper-cli: ${LIBEXEC_DIR}/whisper-cli"
