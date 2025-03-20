####################################################################################################
## Build image
####################################################################################################
FROM rust:latest

WORKDIR /app
RUN apt-get update && apt-get install -y libssl-dev pkg-config gcc clang

# Cache build dependencies
RUN mkdir -p ./src/ && \
         echo "fn main() {}" > ./src/main.rs && \
         echo "fn main() {}" > ./src/lib.rs
COPY ["ds/", "./ds/"]
COPY ["mls_crypto/", "./mls_crypto/"]
COPY ["Cargo.toml", "./Cargo.toml"]
RUN sed -i '/\[\[bench\]\]/,/^\s*$/d' Cargo.toml && \
         cargo build && \
         rm -rf ./src/ 
COPY ["Cargo.toml", "./Cargo.toml"]

# Build the actual app
COPY ["src/", "./src/"]
RUN cargo build

CMD ["/app/target/debug/de-mls"]
