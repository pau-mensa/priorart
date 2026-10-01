# Build from the repository root: docker build -f deploy/Dockerfile .
FROM rust:1.90-trixie AS build
WORKDIR /build
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked && cp target/release/priorart /priorart

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data priorart \
 && install -d -o priorart -g priorart -m 700 /data
COPY --from=build /priorart /usr/local/bin/priorart
USER priorart
WORKDIR /data
ENV PRIORART_DATA_DIR=/data PRIORART_HOST=0.0.0.0 PRIORART_PORT=8000
EXPOSE 8000
ENTRYPOINT ["priorart"]
CMD ["serve"]
