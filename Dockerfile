FROM rust:1.88-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY git-core/ git-core/
COPY git-server/ git-server/
COPY git-remote-near/ git-remote-near/
COPY wasm-lib/ wasm-lib/
COPY res/ res/

RUN cargo build -p git-server --release

FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/git-server /usr/local/bin/git-server
COPY --from=builder /build/res/near_git_storage.wasm /app/res/near_git_storage.wasm

# Download the near-sandbox binary to ~/.near/ where global_install expects it
ARG NEAR_SANDBOX_VERSION=2.10.7
RUN ARCH=$(uname -m) && \
    case $ARCH in \
        aarch64|arm64) ARCH_DIR="Linux-aarch64" ;; \
        x86_64|amd64) ARCH_DIR="Linux-x86_64" ;; \
        *) echo "Unsupported: $ARCH" && exit 1 ;; \
    esac && \
    mkdir -p /root/.near/near-sandbox-${NEAR_SANDBOX_VERSION} && \
    curl -L "https://s3-us-west-1.amazonaws.com/build.nearprotocol.com/nearcore/${ARCH_DIR}/${NEAR_SANDBOX_VERSION}/near-sandbox.tar.gz" \
        | tar -xz -C /tmp && \
    find /tmp -name "near-sandbox" -type f -exec mv {} /root/.near/near-sandbox-${NEAR_SANDBOX_VERSION}/near-sandbox \; && \
    chmod +x /root/.near/near-sandbox-${NEAR_SANDBOX_VERSION}/near-sandbox

WORKDIR /app
ENV LISTEN_ADDR=0.0.0.0:8080

CMD ["git-server"]
