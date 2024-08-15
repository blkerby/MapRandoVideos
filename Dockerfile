FROM rust:1.80.0-bullseye AS build

# Use a dummy binary to build the project dependencies (allowing the results to be cached)
COPY rust/Cargo.lock /rust/Cargo.lock
COPY rust/Cargo.toml /rust/Cargo.toml
COPY rust/src/bin/dummy.rs /rust/src/bin/dummy.rs
WORKDIR /rust
RUN cargo build --release

# Now copy over the source code and build the real binary
COPY /rust/src /rust/src
COPY /rust/templates /rust/templates
RUN cargo build --release

# Now restart with a slim base image and just copy over the binary and data needed at runtime.
FROM debian:bullseye-slim
RUN apt-get update && apt-get install -y \
    libssl1.1 ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /rust/target/release/map-rando-videos /bin/map-rando-videos
COPY --from=build /rust/target/release/video-encoder /bin/video-encoder
COPY --from=build /rust/target/release/sm-json-data-updater /bin/sm-json-data-updater
COPY /js /js
COPY /static /static
WORKDIR /bin
ENTRYPOINT ["/bin/map-rando-videos"]
