# The Linux build and test box for Bottle EQ: Ubuntu 24.04 with PipeWire, its session manager, a
# screen that is not there (Xvfb) and Rust.
#
#   docker build -t bottle-eq-linux linux
#   docker run --rm -v "$PWD":/src -v bottle-eq-target:/target -v bottle-eq-cargo:/root/.cargo/registry bottle-eq-linux linux/test.sh
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl pkg-config clang libclang-dev \
    libpipewire-0.3-dev libspa-0.2-dev pipewire pipewire-bin wireplumber dbus \
    libxkbcommon-dev libxkbcommon-x11-0 libwayland-dev libgl1 libegl1 libgl1-mesa-dri libglx-mesa0 \
    libx11-6 libx11-xcb1 libxcursor1 libxi6 libxrandr2 xvfb \
    && rm -rf /var/lib/apt/lists/*
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH=/root/.cargo/bin:$PATH CARGO_TARGET_DIR=/target
WORKDIR /src
