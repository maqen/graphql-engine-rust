# syntax=docker/dockerfile:1
FROM rust:1.97-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin graphql-engine \
 && cp target/release/graphql-engine /graphql-engine

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /graphql-engine /usr/local/bin/graphql-engine
# Cloud Run sets $PORT; HASURA_GRAPHQL_SERVER_PORT wins when given.
ENV HASURA_GRAPHQL_SERVER_HOST="*"
EXPOSE 8080
# Plain CMD like hasura/graphql-engine, so `command: graphql-engine serve` keeps working.
CMD ["graphql-engine", "serve"]
