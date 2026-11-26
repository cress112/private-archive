FROM rust:1.99.1-slim-bookworm@sha256:f2c004e3d2d8b547e8cf7ffbc29205385f4e71d23e53e4e383065db5e9f822cd
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential cmake perl pkg-config ca-certificates python3 \
    && rm -rf /var/lib/apt/lists/*
ENV CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
WORKDIR /workspace
