#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
CHECKSUM=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef

formula=$(
  TAG=v1.2.3 \
    MACOS_ARM64_SHA256="$CHECKSUM" \
    MACOS_X86_64_SHA256="$CHECKSUM" \
    LINUX_ARM64_SHA256="$CHECKSUM" \
    LINUX_X86_64_SHA256="$CHECKSUM" \
    "$SCRIPT_DIR/render-formula.sh"
)

required_lines=(
  'class PhpRiff < Formula'
  'version "1.2.3"'
  'depends_on "php"'
  'conflicts_with "riff"'
  'riff-v1.2.3-aarch64-apple-darwin.tar.gz'
  'riff-v1.2.3-x86_64-apple-darwin.tar.gz'
  'riff-v1.2.3-aarch64-unknown-linux-gnu.tar.gz'
  'riff-v1.2.3-x86_64-unknown-linux-gnu.tar.gz'
  'generate_completions_from_executable(bin/"riff", "completion")'
)

for line in "${required_lines[@]}"; do
  if ! grep -Fq "$line" <<<"$formula"; then
    echo "Generated formula is missing: $line" >&2
    exit 1
  fi
done

if TAG=v1.2.3 \
  MACOS_ARM64_SHA256=invalid \
  MACOS_X86_64_SHA256="$CHECKSUM" \
  LINUX_ARM64_SHA256="$CHECKSUM" \
  LINUX_X86_64_SHA256="$CHECKSUM" \
  "$SCRIPT_DIR/render-formula.sh" >/dev/null 2>&1
then
  echo "Formula rendering accepted an invalid checksum" >&2
  exit 1
fi

if command -v ruby >/dev/null 2>&1; then
  ruby -c <<<"$formula" >/dev/null
fi
