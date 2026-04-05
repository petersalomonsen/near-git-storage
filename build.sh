#!/bin/bash
set -e

# cargo-near requires Rust 1.86 for wasm compatibility with nearcore VM
RUSTUP_TOOLCHAIN=1.86 cargo near build non-reproducible-wasm

# Build the factory contract
RUSTUP_TOOLCHAIN=1.86 cargo near build non-reproducible-wasm --manifest-path factory/Cargo.toml

# Copy the wasm files to a convenient location
mkdir -p res
cp target/near/near_git_storage.wasm res/
cp target/near/near_git_factory/near_git_factory.wasm res/

echo "Contracts built:"
echo "  res/near_git_storage.wasm"
echo "  res/near_git_factory.wasm"
