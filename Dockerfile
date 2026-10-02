# Build and test environment (Linux). The app itself is a Windows desktop
# program (Tauri); the container runs what CI runs: lint, UI tests, the
# frontend build and the Rust engine tests, with FFmpeg from Debian.
#
#   docker build -t audio-normalizer .                  # runs everything
#   docker build --target frontend -t audio-normalizer-ui .
#
# The image only gets as far as the tests pass; there is nothing to "run".

FROM node:24-bookworm AS node

FROM rust:1-bookworm AS base
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config build-essential \
        libavcodec-dev libavformat-dev libavutil-dev libswresample-dev libavfilter-dev \
        libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev libayatana-appindicator3-dev librsvg2-dev \
    && rm -rf /var/lib/apt/lists/*
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx

# build.rs wants an FFMPEG_DIR with include/, lib/ and bin/. Debian installs
# the headers and libraries system-wide, so point the folders there; bin/ stays
# empty because the shared libraries are found on the system at run time.
ENV FFMPEG_DIR=/opt/ffmpeg
RUN mkdir -p /opt/ffmpeg/bin \
    && ln -s /usr/include /opt/ffmpeg/include \
    && ln -s "/usr/lib/$(gcc -dumpmachine)" /opt/ffmpeg/lib

WORKDIR /app

FROM base AS frontend
COPY package.json package-lock.json ./
COPY tools/lint/package.json tools/lint/package-lock.json tools/lint/
RUN npm ci && npm run lint:install
COPY . .
RUN npm run lint && npm run test:ui && npm run build

FROM frontend AS engine
RUN cargo test --manifest-path backend/Cargo.toml --release
