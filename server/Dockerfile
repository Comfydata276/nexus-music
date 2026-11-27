# The Nexus server as a small image: one static program, its two folders, and nothing else.
# Build from the top of the repository (it links the shared library in core/):
#   docker build -f server/Dockerfile -t nexus-music-server .
# The same Rust version as docker/dev.Dockerfile and the relay.
ARG RUST_VERSION=1.100.0
FROM rust:${RUST_VERSION}-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY core core
COPY server server
RUN cargo build --release --locked --manifest-path server/Cargo.toml \
    && mkdir -p /out/data /out/state

FROM scratch
COPY --from=build /src/server/target/release/nexus-music-server /nexus-music-server
# The data and the store kept apart (design 13.2): mount a folder of the machine's own disk
# on each. They are made here only so that the program finds them if none is mounted.
COPY --from=build --chown=65532:65532 /out/data /data
COPY --from=build --chown=65532:65532 /out/state /state
# Runs as nobody in particular, owning only its two folders.
USER 65532:65532
EXPOSE 8080
# The container's own check: hello on this machine's loopback, and the id it keeps.
HEALTHCHECK --interval=30s --timeout=10s --start-period=10s --retries=3 CMD ["/nexus-music-server", "health"]
# At this slice it holds nothing worth a tidy shutdown, so stopping it is immediate. (When
# it keeps data, it is stopped with a signal it answers by finishing what it is writing.)
STOPSIGNAL SIGKILL
ENTRYPOINT ["/nexus-music-server"]
