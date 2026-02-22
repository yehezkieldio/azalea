FROM lukemathwalker/cargo-chef:latest-rust-1.93.1-bookworm AS chef
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    mold \
    clang \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked -p azalea

FROM debian:bookworm-slim AS downloader
RUN apt-get update && apt-get install -y curl xz-utils ca-certificates
WORKDIR /downloads
RUN curl -L https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux -o yt-dlp && \
    chmod +x yt-dlp
RUN curl -L https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-linux64-gpl.tar.xz | tar xJ && \
    mv ffmpeg-master-latest-linux64-gpl/bin/ffmpeg . && \
    mv ffmpeg-master-latest-linux64-gpl/bin/ffprobe .
RUN mkdir -p /distroless_data /distroless_tmp && \
    chown -R 65532:65532 /distroless_data /distroless_tmp

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
USER nonroot
ENTRYPOINT ["azalea"]

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
ENV LIBVA_DRIVER_NAME=iHD
ENV RUST_LOG=info
ENV PATH="/usr/local/bin:${PATH}"
ENTRYPOINT ["azalea"]