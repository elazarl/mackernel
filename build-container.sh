#!/usr/bin/env bash
# (1) Build the kernel-build container image (latest Ubuntu + gcc-15).
set -euo pipefail
cd "$(dirname "$0")"

IMAGE="${IMAGE:-mackernel-build}"

podman build -t "$IMAGE" -f Containerfile .

echo "=== verifying gcc-15 is present ==="
podman run --rm "$IMAGE" gcc-15 --version | head -1
