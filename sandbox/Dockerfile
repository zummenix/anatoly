# Sandbox image for the anatoly `shell` tool.
#
# Build with either podman or docker (the Dockerfile uses no runtime-specific
# syntax):
#   podman build -f sandbox/Dockerfile -t anatoly-sandbox:0.1 .
#   docker build -f sandbox/Dockerfile -t anatoly-sandbox:0.1 .
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        coreutils \
        findutils \
        gawk \
        git \
        grep \
        gzip \
        python3 \
        sed \
        tar \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /tmp
