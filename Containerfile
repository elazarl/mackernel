# Build environment for compiling a Linux kernel, based on the latest Ubuntu.
# The image ships gcc-15 as the default compiler.
FROM ubuntu:latest

# If gcc-15 is not in the default repos for this Ubuntu release, uncomment the
# two lines below to pull it from the Ubuntu toolchain PPA before installing.
#   RUN apt-get update && apt-get install -y software-properties-common \
#    && add-apt-repository -y ppa:ubuntu-toolchain-r/test

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y \
      gcc-15 build-essential flex bison libssl-dev libelf-dev bc \
      libncurses-dev cpio kmod git \
 && rm -rf /var/lib/apt/lists/*

# Make gcc-15 the default compiler in the image.
RUN update-alternatives --install /usr/bin/gcc gcc /usr/bin/gcc-15 100 \
 && update-alternatives --install /usr/bin/cc  cc  /usr/bin/gcc-15 100

WORKDIR /linux
