# Build environment for compiling a Linux kernel, based on the latest Ubuntu.
# The gcc version is selected at build time via GCC_VERSION (default 15). CI and
# build-container.sh produce one image per version (tagged :<ver>-gcc<N>) so a
# bundle can pick its compiler with a `compiler:` frontmatter key. This matters
# because gcc-15 defaults to C23, which breaks the realmode/boot units of older
# kernels (<~6.7) with "cannot use keyword 'false'"; gcc-13/14 (gnu17) build them.
FROM ubuntu:latest

ARG GCC_VERSION=15
# Auto-set by buildx/podman per build platform ("amd64"/"arm64"; empty for a
# plain host-arch `podman build`). Used below to add the aarch64 cross-toolchain
# only to the amd64 image -- an x86_64 host cross-compiles arm64 kernels with it
# (native compiler speed) instead of emulating the whole arm64 toolchain.
ARG TARGETARCH

# If the requested gcc is not in the default repos for this Ubuntu release,
# uncomment the two lines below to pull it from the Ubuntu toolchain PPA first.
#   RUN apt-get update && apt-get install -y software-properties-common \
#    && add-apt-repository -y ppa:ubuntu-toolchain-r/test

# pkg-config lets perf's `tools:` feature detection run instead of hard-$(error)ing.
# libstdc++-${GCC_VERSION}-dev: perf links -lstdc++, and gcc-${GCC_VERSION} looks only in
# its own lib dir — build-essential's default g++ supplies libstdc++ for gcc-15 only, so
# gcc-14 builds fail "cannot find -lstdc++" without the version-matched dev package (both
# the .so and the .a it needs for the static link — see run-kernel.py build_tools).
# babeltrace2 is deliberately absent: perf is built static and babeltrace2/glib have no
# static archive, so it's disabled via NO_LIBBABELTRACE=1 rather than installed.
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y \
      gcc-${GCC_VERSION} build-essential flex bison libssl-dev libelf-dev bc \
      libncurses-dev cpio kmod git dwarves python3 pkg-config \
      libdw-dev libunwind-dev zlib1g-dev libslang2-dev libcap-dev \
      python3-dev libtraceevent-dev libstdc++-${GCC_VERSION}-dev \
 && if [ "$TARGETARCH" != "arm64" ]; then \
      DEBIAN_FRONTEND=noninteractive apt-get install -y \
        gcc-${GCC_VERSION}-aarch64-linux-gnu; \
    fi \
 && rm -rf /var/lib/apt/lists/*

# Make the selected gcc the default compiler in the image.
RUN update-alternatives --install /usr/bin/gcc gcc /usr/bin/gcc-${GCC_VERSION} 100 \
 && update-alternatives --install /usr/bin/cc  cc  /usr/bin/gcc-${GCC_VERSION} 100

WORKDIR /linux
