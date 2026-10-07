FROM rust:1.99-bookworm@sha256:114c7a4425406451c2866b6aafe69fe29b1b298832db1277d411ac73c82d04d6 AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --locked --release -p temba-catalog

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=build /build/target/release/temba-catalog /temba-catalog
COPY catalog.yaml /etc/temba/catalog.yaml
USER 10001:10001
ENV CATALOG_PATH=/etc/temba/catalog.yaml
EXPOSE 8080
ENTRYPOINT ["/temba-catalog"]
