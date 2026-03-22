ARG YTDLP_TAG=2026.02.21
ARG FFMPEG_TAG=latest
ARG FFMPEG_TARBALL=ffmpeg-master-latest-linux64-gpl.tar.xz

# --- 1. Toolchain (Chef) ---
# We use cargo-chef to cache Rust dependencies separately from source code.
FROM lukemathwalker/cargo-chef:latest-rust-1.93.1-bookworm AS chef
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    mold \
    clang \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# Note: 'mold' and 'clang' are used here as a faster linker alternative to the default 'ld'.

# --- 2. Planner ---
# Creates a 'recipe.json' which is a skeleton of your project (deps only).
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# --- 3. Builder ---
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Cook dependencies: if recipe.json hasn't changed, this layer is cached.
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
# --locked ensures we use the exact versions in Cargo.lock.
# -p azalea targets the specific package in a workspace.
RUN cargo build --release --locked -p azalea

# --- 4. Downloader ---
# A lightweight stage dedicated to fetching external binaries.
FROM debian:bookworm-slim AS downloader
ARG YTDLP_TAG
ARG FFMPEG_TAG
ARG FFMPEG_TARBALL
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl xz-utils ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /downloads

# Download yt-dlp (standalone python-based binary)
RUN curl -L "https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_TAG}/yt-dlp_linux" -o yt-dlp && \
    chmod +x yt-dlp

# Download and extract FFmpeg (Static build)
# --strip-components=2 allows us to grab /bin/ffmpeg directly into the workdir.
RUN set -eux; \
    ffmpeg_url="https://github.com/BtbN/FFmpeg-Builds/releases/download/${FFMPEG_TAG}/${FFMPEG_TARBALL}"; \
    curl --fail --show-error --location "$ffmpeg_url" --output /tmp/ffmpeg.tar.xz; \
    if ! tar xJf /tmp/ffmpeg.tar.xz --strip-components=2 --wildcards "*/bin/ffmpeg" "*/bin/ffprobe"; then \
        echo "Failed to extract FFmpeg archive from: $ffmpeg_url"; \
        exit 1; \
    fi; \
    test -x /downloads/ffmpeg; \
    test -x /downloads/ffprobe; \
    rm -f /tmp/ffmpeg.tar.xz

# --- 5. Runtime Base ---
# Common foundation for both CPU and VA-API targets.
FROM debian:bookworm-slim AS runtime-base
# Create a dedicated system user for security (UID 10001 is a common convention).
RUN groupadd -r azalea && useradd -r -g azalea -u 10001 azalea

# Install CA certs so we can talk to Discord/Websites via HTTPS.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates && rm -rf /var/lib/apt/lists/*

# Pre-prepare persistent and temporary directories with correct ownership.
RUN mkdir -p /data /tmp/azalea && chown -R azalea:azalea /data /tmp/azalea
WORKDIR /data

# Pull in the binaries from previous stages.
COPY --from=downloader /downloads/yt-dlp /usr/local/bin/yt-dlp
COPY --from=downloader /downloads/ffmpeg /usr/local/bin/ffmpeg
COPY --from=downloader /downloads/ffprobe /usr/local/bin/ffprobe
COPY --from=builder /app/target/release/azalea /usr/local/bin/azalea

# Ensure the application knows where to look for logs and binaries.
ENV RUST_LOG=info
ENV PATH="/usr/local/bin:${PATH}"

USER azalea
ENTRYPOINT ["azalea"]

# --- 6. Target: CPU ---
# Pure software transcoding version. Inherits everything from base.
FROM runtime-base AS runtime-cpu

# --- 7. Target: VA-API ---
# Hardware-accelerated version for Intel/AMD GPUs.
FROM runtime-base AS runtime-vaapi
USER root
# Install Intel Media drivers and 'vainfo' for hardware diagnostics.
RUN apt-get update && apt-get install -y --no-install-recommends \
    intel-media-va-driver i965-va-driver vainfo && rm -rf /var/lib/apt/lists/*
USER azalea
