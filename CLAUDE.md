# CLAUDE.md

## Build

```bash
# Build contracts (requires Rust 1.86 for NEAR VM compatibility)
./build.sh

# Outputs:
#   res/near_git_storage.wasm  — git repo contract
#   res/near_git_factory.wasm  — factory contract (with web4 UI)
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

- **`src/lib.rs`** — Git storage contract (stores refs, object types, object data, tx mappings)
- **`factory/`** — Factory contract: deploys repos as sub-accounts using global contracts, serves web4 UI
- **`git-remote-near/`** — Git remote helper (`git push/clone near://repo.factory.testnet`)
- **`git-server/`** — HTTP git server for sandbox testing (deploys factory + global contract on startup)
- **`git-core/`** — Shared packfile parsing/building library
- **`wasm-lib/`** — Browser WASM module for packfile ops + NEAR tx signing
- **`e2e/`** — Playwright tests + web frontend (blog demo, testnet page, create-repo)
  - `e2e/public/near-git-sw.js` — Service worker that intercepts git HTTP and translates to NEAR RPC
  - `e2e/serve.mjs` — Dev server with NEAR RPC proxy

## Key design decisions

- Repo contracts are sub-accounts of the factory (e.g. `myrepo.gitfactory.testnet`)
- Repo accounts have **no access keys** — only contract methods can interact
- `new()` verifies predecessor is the parent account (factory), owner is `signer_account_id`
- Factory uses `use_global_contract_by_account_id` — no WASM stored per repo
- Two-step push: `push_objects` (stores data) → `register_push` (stores SHA→tx_hash, updates refs)

## Testnet deployments

- `gitglobal.testnet` — global git-storage contract code
- `gitfactory.testnet` — factory contract
- Web4 UI: https://gitfactory.testnet.page/
- Cloudflare Pages: https://near-git-storage.pages.dev/create-repo

## Dependencies

- near-sdk 5.17 with `global-contracts` feature (for factory)
- Contracts must build with Rust 1.86 (nearcore VM compatibility)
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
