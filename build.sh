#!/bin/bash
set -e

echo "Building compatible Wasm..."
RUSTFLAGS='-C target-feature=-bulk-memory' cargo build --release --target wasm32-unknown-unknown