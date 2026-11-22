# Bookworm ships an older glibc than a typical host — which is the whole reason
# lodge exists, so this devcontainer doubles as a realistic target to dogfood it.
FROM rust:1-bookworm

# The Windows target and mingw (`ring` builds C and assembly) are for type-checking
# the Windows client from here: `cargo clippy --target x86_64-pc-windows-gnu`.
RUN rustup component add clippy rustfmt \
    && rustup target add x86_64-pc-windows-gnu \
    && apt-get update -qq \
    && apt-get install -y -qq --no-install-recommends patchelf gcc-mingw-w64-x86-64 \
    && rm -rf /var/lib/apt/lists/*

# The docker CLI and its compose plugin (static binaries), for the integration
# tests against the host's engine (see compose.yaml).
COPY --from=docker:27-cli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=docker:27-cli /usr/local/libexec/docker/cli-plugins /usr/local/libexec/docker/cli-plugins

ARG UID=1000
RUN useradd -m -u $UID dev \
    && mkdir -p /workspace /usr/local/cargo \
    && chown -R dev:dev /workspace /usr/local/cargo /usr/local/rustup

USER dev
WORKDIR /workspace
