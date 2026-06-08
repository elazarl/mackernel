#!/usr/bin/env bash
# (1) Build the kernel-build container image (latest Ubuntu + gcc-15).
set -euo pipefail
cd "$(dirname "$0")"
source ./lib.sh

IMAGE="${IMAGE:-mackernel-build}"

# Tag with both a floating tag and the pinned version (from VERSION).
podman build -t "$IMAGE" -t "${IMAGE}:${MACKERNEL_VERSION}" -f Containerfile .

echo "=== verifying gcc-15 is present (mackernel ${MACKERNEL_VERSION}) ==="
podman run --rm "$IMAGE" gcc-15 --version | head -1
