# CLAUDE.md

## Build

```bash
# Build contracts (requires Rust 1.86 for NEAR VM compatibility)
# Default FEE_RECIPIENT=gitfactory.testnet; for mainnet: FEE_RECIPIENT=gitfactory.near ./build.sh
./build.sh

# Outputs:
#   res/near_git_storage.wasm  — git repo contract (packfile store)
#   res/near_git_factory.wasm  — factory contract (with web4 UI)

# Build WASM module for browser
cd wasm-lib && wasm-pack build --target web
```

## Test

```bash
# Rust integration + factory tests (starts sandbox automatically)
RUSTUP_TOOLCHAIN=stable cargo test --test integration --test factory

# Playwright e2e tests (starts git-server + sandbox + web server)
# Kill stale processes first: pkill -f near-sandbox; pkill -f git-server
cd e2e && npx playwright test

# Single e2e test
npx playwright test tests/service-worker.spec.js
npx playwright test tests/create-repo.spec.js
```

## Architecture

- **`src/lib.rs`** — Git storage contract (stores packfiles verbatim, refs with CAS)
- **`factory/`** — Factory contract: deploys repos as sub-accounts using hash-based global contracts, serves web4 UI
- **`git-remote-near/`** — Git remote helper (`git push/clone near://repo.factory.testnet`), uses `git pack-objects --thin` for delta compression
- **`git-server/`** — HTTP git server for sandbox testing (deploys factory + global contract on startup)
- **`git-core/`** — Shared library: packfile parse/build with OFS_DELTA + REF_DELTA support, delta compression, zlib
- **`wasm-lib/`** — Browser WASM module: packfile ops, delta-aware pack building, NEAR tx signing
- **`e2e/`** — Playwright tests + web frontend (blog demo, testnet page, create-repo)
  - `e2e/public/near-git-sw.js` — Service worker that intercepts git HTTP and translates to NEAR RPC
  - `e2e/serve.mjs` — Dev server with NEAR RPC proxy

## Key design decisions

- Contract is a minimal packfile store — client handles all compression/delta logic
- Packfiles stored verbatim (one per push), fetched with `get_packs(from_index)`
- git-remote-near uses `git pack-objects --thin --revs` for native delta compression
- Service worker uses `build_packfile_with_bases()` for cross-pack delta compression
- Repo contracts are sub-accounts of the factory (e.g. `myrepo.gitfactory.testnet`)
- Repo accounts have **no access keys** — only contract methods can interact
- `new()` transfers 0.1 NEAR service fee to FEE_RECIPIENT (build-time env var)
- Factory uses `use_global_contract(hash)` — repos pinned to exact code version
- Single-step push: `push(pack_data, ref_updates)` stores packfile + updates refs

## Testnet deployments

- `gitfactory.testnet` — factory contract (deploys global contract by hash)
- Web4 UI: https://gitfactory.testnet.page/
- Cloudflare Pages: https://near-git-storage.pages.dev/create-repo

## Dependencies

- near-sdk 5.17 with `global-contracts` feature (for factory)
- Contracts must build with Rust 1.86 (nearcore VM compatibility)
- FEE_RECIPIENT env var set at build time (default: sandbox via .cargo/config.toml)
- git-server/git-remote-near use stable Rust
- `cargo-near` 0.17+ for contract builds (installed via pre-built binary in CI)

## CI

- `.github/workflows/test.yml` — e2e Playwright tests
- `.github/workflows/docker.yml` — Multi-arch Docker image → `ghcr.io/petersalomonsen/near-git-storage/sandbox`
- Squash merge only (configured in repo settings)

## Install git-remote-near

```bash
RUSTUP_TOOLCHAIN=stable cargo install --path git-remote-near
```
