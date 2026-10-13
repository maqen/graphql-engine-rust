# syntax=docker/dockerfile:1
FROM rust:1.97-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin graphql-engine \
 && cp target/release/graphql-engine /graphql-engine

# pg_dump for /v1alpha1/pg_dump (`hasura migrate create --from-server`, `hasura seed create
# --from-table`), the major version V2 v2.51 ships, with the shared libraries it needs (glibc
# comes with the runtime image) at their own paths so the runtime's loader finds them.
FROM postgres:18-bookworm AS pgdump
RUN mkdir -p /pg/usr/local/bin \
 && cp /usr/lib/postgresql/18/bin/pg_dump /pg/usr/local/bin/ \
 && ldd /usr/lib/postgresql/18/bin/pg_dump | awk '$3 ~ /^\// {print $3}' \
    | grep -vE '/(libc|libm|libdl|libpthread|librt|libresolv)\.so' \
    | while read -r lib; do mkdir -p "/pg$(dirname "$lib")" && cp -L "$lib" "/pg$lib"; done

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=pgdump /pg/ /
COPY --from=build /graphql-engine /usr/local/bin/graphql-engine
# Cloud Run sets $PORT; HASURA_GRAPHQL_SERVER_PORT wins when given.
ENV HASURA_GRAPHQL_SERVER_HOST="*"
EXPOSE 8080
# Plain CMD like hasura/graphql-engine, so `command: graphql-engine serve` keeps working.
CMD ["graphql-engine", "serve"]
