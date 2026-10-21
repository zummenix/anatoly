# Container runtime used to build/run the sandbox image. Override with
# `make sandbox-image RUNTIME=docker`.
RUNTIME ?= podman
IMAGE ?= anatoly-sandbox:0.1

.PHONY: sandbox-image
sandbox-image:
	$(RUNTIME) build -f sandbox/Dockerfile -t $(IMAGE) .
