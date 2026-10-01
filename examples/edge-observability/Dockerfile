# syntax=docker/dockerfile:1
# End-to-end demo images. Build context: the repository root.
# Every base image is pinned by digest; see docs/compatibility.md.

# Ferrum Edge image for the edge stage. The default is the contract baseline:
# Ferrum Edge v0.9.10 (published multi-arch index).
# CI sets EDGE_IMAGE through compose.yaml to test every release in the support
# window (docs/compatibility.json).
ARG EDGE_IMAGE=ferrumedge/ferrum-edge@sha256:430d6a7d41361de5ad12562786481f97f1e97fef72a0b5f1a0699eced7cdd4cc

FROM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS build
WORKDIR /src
COPY . .
RUN cargo build --locked -p example-edge-observability -p ferrum-alloy-cli --bins
RUN mkdir -p /out/certs /out/collector \
 && ./target/debug/gen-demo-certs /out/certs \
 && chmod 0644 /out/certs/* \
 && ./target/debug/ferrum-alloy check --no-env --config examples/edge-observability/alloy.toml --features otel,tls,edge \
 && ./target/debug/ferrum-alloy edge export --manifest examples/edge-observability/ferrum-service.toml --output /out/edge.export.yaml \
 && ./target/debug/gen-e2e-edge-config examples/edge-observability/ferrum-service.toml /out/edge.export.yaml /out/edge.yaml

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS alloy
COPY --from=build /src/target/debug/edge-demo-api /usr/local/bin/edge-demo-api
COPY --from=build /out/certs /certs
COPY examples/edge-observability/alloy.toml /etc/alloy/alloy.toml
ENV FERRUM_ALLOY_CONFIG=/etc/alloy/alloy.toml
USER 65532:65532
EXPOSE 8443
ENTRYPOINT ["/usr/local/bin/edge-demo-api"]

FROM ${EDGE_IMAGE} AS edge
COPY --from=build --chown=65532:65532 /out/certs /certs
COPY --from=build --chown=65532:65532 /out/edge.yaml /config/edge.yaml

FROM otel/opentelemetry-collector-contrib:0.161.0@sha256:fd328de2552466ad78385e1b1289c3f2402b1c45f265b252aab1955b42845ac1 AS collector
COPY --from=build --chown=10001:10001 /out/collector /out
COPY examples/edge-observability/collector.yaml /etc/otelcol-contrib/config.yaml
