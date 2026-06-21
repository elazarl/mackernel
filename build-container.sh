#!/usr/bin/env bash
# (1) Build the kernel-build container image(s) (latest Ubuntu + gcc). One image
# per gcc version so a bundle can pick its compiler via the `compiler:` key.
# gcc-15 is the default and gets the plain tag (mackernel-build); others get a
# -gcc<N> suffix, matching mklib.resolve_image. Set GCC_VERSIONS to override the
# list, PUSH=1 to also push the :<version>-gcc<N> tags to GHCR (needs login;
# CI normally publishes the multi-arch images).
set -euo pipefail
cd "$(dirname "$0")"
source ./lib.sh

IMAGE="${IMAGE:-mackernel-build}"
GCC_VERSIONS="${GCC_VERSIONS:-13 14 15}"
REMOTE="${REMOTE_IMAGE_BASE:-ghcr.io/elazarl/mackernel}"

for v in $GCC_VERSIONS; do
  # gcc-15 (default) gets the plain local tag; others get a -gcc<N> suffix.
  if [ "$v" = "15" ]; then tag="$IMAGE"; else tag="${IMAGE}-gcc${v}"; fi
  echo "=== building $tag (gcc-$v, mackernel ${MACKERNEL_VERSION}) ==="
  podman build --build-arg "GCC_VERSION=$v" \
    -t "$tag" -t "${tag}:${MACKERNEL_VERSION}" -f Containerfile .
  podman run --rm "$tag" "gcc-$v" --version | head -1

  if [ "${PUSH:-0}" = "1" ]; then
    rtag="${REMOTE}:${MACKERNEL_VERSION}-gcc${v}"
    echo "=== pushing $rtag ==="
    podman tag "$tag" "$rtag" && podman push "$rtag"
    if [ "$v" = "15" ]; then
      podman tag "$tag" "${REMOTE}:${MACKERNEL_VERSION}" && podman push "${REMOTE}:${MACKERNEL_VERSION}"
    fi
  fi
done
