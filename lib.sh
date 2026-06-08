#!/usr/bin/env bash
# Shared helpers for the mackernel scripts.

_here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Version string; also the tag used for the published GHCR image.
MACKERNEL_VERSION="$(cat "$_here/VERSION" 2>/dev/null || echo latest)"

# Locally-built image tag (produced by build-container.sh).
LOCAL_IMAGE="${IMAGE:-mackernel-build}"

# Prebuilt image published to GHCR by the CI workflow. Override REMOTE_IMAGE to
# point elsewhere (e.g. a fork). Defaults to this repo's package at the pinned version.
REMOTE_IMAGE="${REMOTE_IMAGE:-ghcr.io/elazarl/mackernel:${MACKERNEL_VERSION}}"

# Echo the image reference to use: prefer the locally-built image, otherwise fall
# back to the GHCR image (podman pulls it automatically on first use).
resolve_image() {
  if podman image exists "$LOCAL_IMAGE" 2>/dev/null; then
    echo "$LOCAL_IMAGE"
  else
    echo "$REMOTE_IMAGE"
  fi
}
