####################################################################################################
## Build image
####################################################################################################
FROM rust:latest as builder

WORKDIR /app
RUN apt-get update && apt-get install -y libssl-dev pkg-config gcc clang

ENV PATH="/usr/local/go/bin:${PATH}"
COPY --from=golang:1.20 /usr/local/go/ /usr/local/go/

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

CMD ["/app/target/release/de-mls"]
