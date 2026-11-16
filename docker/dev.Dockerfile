# Build and test environment for Nexus Music.
# - Linux build + headless UI testing (Xvfb, software GL, xdotool)
# - Windows x64 cross-compile (cargo-xwin, clang-cl, lld-link) and NSIS installer
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl git build-essential pkg-config cmake ninja-build python3 \
      clang lld llvm \
      libasound2-dev libfontconfig1-dev libfreetype-dev libxkbcommon-dev libxkbcommon-x11-0 \
      libx11-dev libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
      libxcursor1 libxrandr2 libxi6 libx11-xcb1 libwayland-dev libdbus-1-dev libssl-dev \
      libgl1-mesa-dri libgl1 libegl1 libgles2 mesa-utils \
      xvfb xdotool imagemagick x11-utils alsa-utils \
      nsis zip \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable \
      --target x86_64-pc-windows-msvc \
    && cargo install cargo-xwin --locked \
    && chmod -R a+rwX /opt/rustup /opt/cargo

# Pre-fetch the Windows SDK/CRT so builds don't download it each time.
ENV XWIN_CACHE_DIR=/opt/xwin
RUN mkdir -p /opt/xwin && cargo xwin cache xwin && chmod -R a+rwX /opt/xwin

WORKDIR /work
