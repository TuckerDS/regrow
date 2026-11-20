#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# SPDX-FileCopyrightText: 2026 TuckerDS
# Construye los ejecutables publicables — uno simple por SO — en build/dist/.
#
#   regrow-macos            macOS x86_64 (también corre en Apple Silicon vía Rosetta)
#   regrow-linux            Linux x86_64 estático (musl): vale en cualquier distro
#   regrow-windows-x64.exe  Windows 10/11 x64, un solo archivo sin dependencias
#
# Requisitos opcionales (quien no los tenga simplemente no genera ese binario):
#   brew install mingw-w64
#   brew install FiloSottile/musl-cross/musl-cross
#
# Uso: scripts/dist.sh [--with-arm64]   (el flag añade regrow-macos-arm64 nativo)
set -euo pipefail
cd "$(dirname "$0")/.."
out=build/dist
mkdir -p "$out"

# macOS (Intel)
cargo build --release
cp build/release/regrow "$out/regrow-macos"

# macOS Apple Silicon nativo (opcional)
if [[ "${1:-}" == "--with-arm64" ]]; then
    cargo build --release --target aarch64-apple-darwin
    cp build/aarch64-apple-darwin/release/regrow "$out/regrow-macos-arm64"
fi

# Linux estático con musl (sin atarse a la glibc de la máquina de build)
if command -v x86_64-linux-musl-gcc >/dev/null 2>&1; then
    cargo build --release --target x86_64-unknown-linux-musl
    cp build/x86_64-unknown-linux-musl/release/regrow "$out/regrow-linux"
else
    echo "aviso: sin musl-cross (brew install FiloSottile/musl-cross/musl-cross); no se genera regrow-linux" >&2
fi

# Windows vía mingw-w64
if command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
    cargo build --release --target x86_64-pc-windows-gnu
    cp build/x86_64-pc-windows-gnu/release/regrow.exe "$out/regrow-windows-x64.exe"
else
    echo "aviso: sin mingw-w64; no se genera regrow-windows-x64.exe" >&2
fi

echo
ls -lh "$out"
echo
echo "para colgarlos en un Release (cuando el autor lo apruebe):"
echo "  gh release create vX.Y.Z $out/*"
