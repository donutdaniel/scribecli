#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
VERSION_FILE="${SCRIPT_DIR}/whisper_cpp_version.txt"
WHISPER_CPP_VERSION="${WHISPER_CPP_VERSION:-$(tr -d '[:space:]' < "${VERSION_FILE}")}"

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <output-binary-path>" >&2
  exit 1
fi

OUTPUT_PATH="$1"
WORK_DIR="${WHISPER_BUILD_DIR:-${REPO_ROOT}/.build/whispercpp-${WHISPER_CPP_VERSION}}"
SOURCE_DIR="${WORK_DIR}/source"
BUILD_DIR="${WORK_DIR}/build"
ARCHIVE_PATH="${WORK_DIR}/whispercpp.tar.gz"

if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required to fetch whisper.cpp source" >&2
  exit 1
fi

if ! command -v cmake >/dev/null 2>&1; then
  echo "cmake is required to build bundled whisper-cli; install it first, for example: brew install cmake" >&2
  exit 1
fi

mkdir -p "${WORK_DIR}" "$(dirname "${OUTPUT_PATH}")"

if [[ ! -f "${ARCHIVE_PATH}" ]]; then
  curl -fsSL \
    -o "${ARCHIVE_PATH}" \
    "https://github.com/ggml-org/whisper.cpp/archive/refs/tags/v${WHISPER_CPP_VERSION}.tar.gz"
fi

rm -rf "${SOURCE_DIR}" "${BUILD_DIR}"
mkdir -p "${SOURCE_DIR}"
tar -xzf "${ARCHIVE_PATH}" -C "${SOURCE_DIR}" --strip-components=1

cmake -S "${SOURCE_DIR}" -B "${BUILD_DIR}" -DCMAKE_BUILD_TYPE=Release
cmake --build "${BUILD_DIR}" --config Release --target whisper-cli -j

BUNDLED_CLI="$(find "${BUILD_DIR}" -type f -name whisper-cli | head -n 1)"
if [[ -z "${BUNDLED_CLI}" || ! -f "${BUNDLED_CLI}" ]]; then
  echo "failed to locate built whisper-cli under ${BUILD_DIR}" >&2
  exit 1
fi

cp "${BUNDLED_CLI}" "${OUTPUT_PATH}"
chmod +x "${OUTPUT_PATH}"
codesign --force --sign - "${OUTPUT_PATH}" >/dev/null 2>&1 || true

echo "${OUTPUT_PATH}"
