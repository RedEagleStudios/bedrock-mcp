# Cross toolchain for the Windows crates (agent DLL and injector).
FROM rust:1-bookworm
RUN apt-get update \
    && apt-get install -y --no-install-recommends gcc-mingw-w64-x86-64 \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-pc-windows-gnu \
    && rustup component add clippy rustfmt
