ARG RUST_VERSION=1.93
ARG ALPINE_VERSION=3.22
ARG TAILSCALE_VERSION=1.94.1

FROM rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS chef
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static
RUN --mount=type=cache,target=/usr/local/cargo/registry \
	--mount=type=cache,target=/usr/local/cargo/git \
	cargo install cargo-chef
WORKDIR /pluribus

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /pluribus/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
	--mount=type=cache,target=/usr/local/cargo/git \
	cargo chef cook --release --recipe-path recipe.json --package pluribus-unum
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
	--mount=type=cache,target=/usr/local/cargo/git \
	cargo build --release --locked --package pluribus-unum

FROM ghcr.io/tailscale/tailscale:v${TAILSCALE_VERSION} as tailscale

FROM alpine:${ALPINE_VERSION}
ARG S6_OVERLAY_VERSION=3.2.1.0
ARG TARGETARCH
ADD "https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-noarch.tar.xz" /tmp
RUN S6_ARCH=$(case "${TARGETARCH}" in "amd64") echo "x86_64" ;; "arm64") echo "aarch64" ;; *) echo "${TARGETARCH}" ;; esac) && \
	wget -O /tmp/s6-overlay-arch.tar.xz "https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-${S6_ARCH}.tar.xz" && \
	tar -C / -Jxpf /tmp/s6-overlay-noarch.tar.xz && \
	tar -C / -Jxpf /tmp/s6-overlay-arch.tar.xz && \
	rm -rf /tmp/*
COPY init-wrapper /
COPY --from=tailscale /usr/local/bin/tailscale  /usr/local/bin/tailscale
COPY --from=tailscale /usr/local/bin/tailscaled /usr/local/bin/tailscaled
RUN addgroup -S pluribus && \
	adduser -S -G pluribus -h /home/unum unum && \
	mkdir -p /data && \
	chown -R unum:pluribus /home/unum /data
COPY --from=builder /pluribus/target/release/unum /usr/local/bin/unum
COPY etc/ /etc/
ENTRYPOINT ["/init-wrapper"]
