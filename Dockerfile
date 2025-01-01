FROM rust:1.80.0-bookworm AS build

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
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y \
    libssl3 wget xz-utils ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /
RUN wget https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-amd64-static.tar.xz \
  && tar xf ffmpeg-release-amd64-static.tar.xz \
  && cp ffmpeg-7.0.2-amd64-static/ffmpeg /bin/ffmpeg \
  && rm ffmpeg-release-amd64-static.tar.xz \
  && rm -rf ffmpeg-7.0.2-amd64-static
COPY --from=build /rust/target/release/map-rando-videos /app/map-rando-videos
COPY --from=build /rust/target/release/video-encoder /app/video-encoder
COPY --from=build /rust/target/release/sm-json-data-updater /app/sm-json-data-updater
COPY --from=build /rust/target/release/trigger-encode-all /app/trigger-encode-all
COPY /js /js
COPY /css /css
COPY /static /static
WORKDIR /app
ENTRYPOINT ["/app/map-rando-videos"]
