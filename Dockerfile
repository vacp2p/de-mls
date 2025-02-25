####################################################################################################
## Build image
####################################################################################################
FROM rust:latest

WORKDIR /app
RUN apt-get update && apt-get install -y libssl-dev pkg-config gcc clang

# Cache build dependencies
RUN echo "fn main() {}" > dummy.rs
COPY ["Cargo.toml", "./Cargo.toml"]
COPY ["ds/", "./ds/"]
COPY ["mls_crypto/", "./mls_crypto/"]
RUN sed -i 's#src/main.rs#dummy.rs#' Cargo.toml
RUN cargo build --release
RUN sed -i 's#dummy.rs#src/main.rs#' Cargo.toml

# Build the actual app
COPY ["src/", "./src/"]
RUN cargo build --release

CMD ["/app/target/release/de_mls"]
