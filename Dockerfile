# --- Base stage: shared toolchain ---
FROM lukemathwalker/cargo-chef:latest-rust-1.93.1-bookworm AS chef
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    mold \
    clang \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# --- Planner: compute the dependency recipe from the workspace manifest ---
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# --- Builder: cook (pre-build) deps from the recipe, then compile the binary ---
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Dependencies are built separately so they are cached across source-only changes.
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked -p azalea

# --- Downloader: fetch third-party binaries (yt-dlp, ffmpeg) in a disposable layer ---
FROM debian:bookworm-slim AS downloader
RUN apt-get update && apt-get install -y curl xz-utils ca-certificates
WORKDIR /downloads

# TODO: Pin yt-dlp and ffmpeg versions instead of always fetching the latest.
RUN curl -L https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux -o yt-dlp && \
    chmod +x yt-dlp
RUN curl -L https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-linux64-gpl.tar.xz | tar xJ && \
    mv ffmpeg-master-latest-linux64-gpl/bin/ffmpeg . && \
    mv ffmpeg-master-latest-linux64-gpl/bin/ffprobe .

# Pre-create /data and /tmp/azalea owned by distroless nonroot UID 65532.
# These directories must exist before the COPY in the runtime-cpu stage.
RUN mkdir -p /distroless_data /distroless_tmp && \
    chown -R 65532:65532 /distroless_data /distroless_tmp

# --- Runtime (CPU): minimal distroless image, no shell, no package manager ---
FROM gcr.io/distroless/cc-debian12 AS runtime-cpu
COPY --from=downloader --chown=65532:65532 /distroless_data /data
COPY --from=downloader --chown=65532:65532 /distroless_tmp /tmp/azalea
WORKDIR /data
COPY --from=downloader /downloads/yt-dlp /usr/local/bin/yt-dlp
COPY --from=downloader /downloads/ffmpeg /usr/local/bin/ffmpeg
COPY --from=downloader /downloads/ffprobe /usr/local/bin/ffprobe
COPY --from=builder /app/target/release/azalea /usr/local/bin/azalea
ENV RUST_LOG=info
ENV PATH="/usr/local/bin:${PATH}"
# distroless nonroot user (UID 65532)
USER nonroot
ENTRYPOINT ["azalea"]

# --- Runtime (VA-API): Debian slim with Intel/i965 VA-API drivers for hardware-accelerated transcoding ---
FROM debian:bookworm-slim AS runtime-vaapi
RUN groupadd -r azalea && useradd -r -g azalea -u 10001 azalea
RUN apt-get update && apt-get install -y --no-install-recommends \
    intel-media-va-driver \
    i965-va-driver \
    vainfo \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /data /tmp/azalea && chown -R azalea:azalea /data /tmp/azalea
WORKDIR /data
COPY --from=downloader /downloads/yt-dlp /usr/local/bin/yt-dlp
COPY --from=downloader /downloads/ffmpeg /usr/local/bin/ffmpeg
COPY --from=downloader /downloads/ffprobe /usr/local/bin/ffprobe
COPY --from=builder /app/target/release/azalea /usr/local/bin/azalea
USER azalea
ENV RUST_LOG=info
ENV PATH="/usr/local/bin:${PATH}"
ENTRYPOINT ["azalea"]