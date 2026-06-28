# Alpine (musl) works here because gRPC to headscale runs as plain HTTP inside
# the cluster (no system TLS library needed) and Kubernetes API calls use rustls
# with bundled CA certs — no system OpenSSL required.
FROM docker.io/library/rust:1.96-alpine AS chef
WORKDIR /build
RUN apk add --no-cache musl-dev git && cargo install cargo-chef

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
# Build deps only — this layer is GHA-cached when Cargo.lock is unchanged
COPY --from=planner /build/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry/ \
    cargo chef cook --release --recipe-path recipe.json

# Build binary — deps already compiled above, only source recompiles
COPY Cargo.toml Cargo.lock ./
COPY operator/ operator/
COPY headscale-client/ headscale-client/
COPY scim/ scim/
COPY integration-tests/ integration-tests/
COPY .git/ .git/
RUN --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry/ \
    cargo build --locked --release --bin operator --bin headmaster-scim && \
    cp ./target/release/operator /bin/headmaster && \
    cp ./target/release/headmaster-scim /bin/headmaster-scim

FROM scratch AS final
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=builder /bin/headmaster /usr/local/bin/headmaster
COPY --from=builder /bin/headmaster-scim /usr/local/bin/headmaster-scim
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/headmaster"]
