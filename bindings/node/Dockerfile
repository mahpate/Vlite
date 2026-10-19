# syntax=docker/dockerfile:1
FROM node:22-bookworm-slim AS node
FROM rust:1.89-bookworm AS build
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
RUN ln -s /usr/local/lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
COPY examples ./examples
COPY bindings/c ./bindings/c
COPY bindings/node ./bindings/node
RUN npm --prefix bindings/node ci --ignore-scripts \
    && npm --prefix bindings/node run build \
    && npm --prefix bindings/node test \
    && npm --prefix bindings/node run typecheck \
    && node bindings/node/pack.mjs /packages \
    && node tests/node_package.mjs /packages/*.tgz

FROM node AS verify
COPY --from=build /packages /packages
COPY tests/node_package.mjs /test.mjs
RUN --network=none node /test.mjs /packages/*.tgz

FROM scratch AS artifacts
COPY --from=verify /packages/ /
