# Alpine (musl) works here because gRPC to headscale runs as plain HTTP inside
# the cluster (no system TLS library needed) and Kubernetes API calls use rustls
# with bundled CA certs — no system OpenSSL required.
FROM docker.io/library/rust:1.96-alpine AS build
WORKDIR /build

RUN apk add --no-cache musl-dev git

COPY Cargo.toml Cargo.lock ./
COPY operator/ operator/
COPY headscale-client/ headscale-client/
COPY scim/ scim/
COPY integration-tests/ integration-tests/
COPY .git/ .git/

RUN --mount=type=cache,target=/build/target/,id=headmaster-target \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry/ \
    cargo build --locked --release --bin operator --bin headmaster-scim && \
    cp ./target/release/operator /bin/headmaster && \
    cp ./target/release/headmaster-scim /bin/headmaster-scim

FROM scratch AS final
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=build /bin/headmaster /usr/local/bin/headmaster
COPY --from=build /bin/headmaster-scim /usr/local/bin/headmaster-scim
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/headmaster"]
