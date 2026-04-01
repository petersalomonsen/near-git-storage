#!/bin/bash
set -e

# cargo-near requires Rust 1.86 for wasm compatibility with nearcore VM
RUSTUP_TOOLCHAIN=1.86 cargo near build non-reproducible-wasm

# Copy the wasm file to a convenient location
mkdir -p res
cp target/near/near_git_storage.wasm res/

echo "Contract built: res/near_git_storage.wasm"
