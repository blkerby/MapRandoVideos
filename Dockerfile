FROM rust:1.79.0-bullseye AS build

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
FROM debian:buster-slim
RUN apt-get update && apt-get install -y \
    libssl1.1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /rust/target/release/map-rando-videos /
COPY /js /js
WORKDIR /
ENTRYPOINT ["/map-rando-videos"]
