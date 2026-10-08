# Sandbox image for the anatoly `shell` tool.
#
# Build with either podman or docker (the Dockerfile uses no runtime-specific
# syntax):
#   podman build -f sandbox/Dockerfile -t anatoly-sandbox:0.1 .
#   docker build -f sandbox/Dockerfile -t anatoly-sandbox:0.1 .
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        coreutils \
        curl \
        findutils \
        gawk \
        git \
        grep \
        gzip \
        libssl-dev \
        nodejs \
        npm \
        pkg-config \
        python3 \
        python-is-python3 \
        python3-pip \
        python3-venv \
        sed \
        tar \
        yarnpkg \
    && rm -rf /var/lib/apt/lists/*

RUN ln -s /usr/bin/yarnpkg /usr/local/bin/yarn \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup-init.sh \
    && CARGO_HOME=/usr/local/cargo RUSTUP_HOME=/usr/local/rustup \
        sh /tmp/rustup-init.sh -y --profile minimal --default-toolchain 1.98.1 --no-modify-path \
    && rm /tmp/rustup-init.sh

# The root filesystem is read-only at runtime, so per-user installs (cargo,
# npm -g) must target the writable /tmp tmpfs; put those bin dirs on PATH too.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/tmp/.cargo \
    npm_config_prefix=/tmp/npm \
    PATH=/tmp/.cargo/bin:/tmp/npm/bin:/usr/local/cargo/bin:$PATH

WORKDIR /tmp
