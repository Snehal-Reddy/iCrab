#!/usr/bin/env bash
# Build iCrab for iSH (i686-unknown-linux-musl) using cross-rs.
# Requires: cargo install cross, Docker.
# Usage: ./build.sh [--release] [cargo build args...]
set -euo pipefail
cd "$(dirname "$0")"
exec cross build "$@"
