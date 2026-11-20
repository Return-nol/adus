# syntax=docker/dockerfile:1

# 1. Dashboard: SvelteKit static build (web/build). The Rust binary embeds it at compile time.
FROM node:22-bookworm AS web
WORKDIR /app/web
RUN corepack enable && corepack prepare pnpm@11 --activate
COPY web/package.json web/pnpm-lock.yaml web/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm build

# 2. Rust dependencies, cached by cargo-chef: this layer only rebuilds when a Cargo.toml/Cargo.lock changes.
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates crates
RUN cargo chef prepare --recipe-path recipe.json

# 3. The adus binary.
FROM chef AS build
COPY rust-toolchain.toml ./
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release -p adus --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY migrations migrations
COPY --from=web /app/web/build web/build
RUN cargo build --release -p adus

# 4. Runtime image.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --no-create-home --shell /usr/sbin/nologin adus
COPY --from=build /app/target/release/adus /usr/local/bin/adus
USER adus
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 CMD ["adus", "healthcheck"]
ENTRYPOINT ["adus"]
CMD ["serve"]
