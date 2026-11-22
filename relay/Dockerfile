# The jam relay as a small image: one static program and nothing else.
# Build from the top of the repository (the page's icon lives outside this folder):
#   docker build -f relay/Dockerfile -t nexus-music-jam .
# The same Rust version as docker/dev.Dockerfile.
ARG RUST_VERSION=1.100.0
FROM rust:${RUST_VERSION}-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY assets/app-icon.png assets/app-icon.png
COPY relay relay
RUN cargo build --release --locked --manifest-path relay/Cargo.toml

FROM scratch
COPY --from=build /src/relay/target/release/nexus-music-jam /nexus-music-jam
# Runs as nobody in particular: it reads and writes no files.
USER 65532:65532
EXPOSE 8080
# It holds nothing worth a tidy shutdown, so stopping it is immediate.
STOPSIGNAL SIGKILL
ENTRYPOINT ["/nexus-music-jam"]
