FROM node:22-bookworm-slim AS frontend
WORKDIR /src
COPY controlplane/frontend/package*.json ./
RUN npm ci
COPY controlplane/frontend/ ./
RUN npm run build

FROM rust:1.100-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked --workspace

FROM debian:bookworm-slim AS runtime
WORKDIR /lab
USER 65532:65532

FROM runtime AS proxy
COPY --from=build /src/target/release/proxy /usr/local/bin/proxy
COPY proxy/config/docker.toml /lab/proxy.toml
ENTRYPOINT ["/usr/local/bin/proxy", "/lab/proxy.toml"]

FROM runtime AS appserver
COPY --from=build /src/target/release/appserver /usr/local/bin/appserver
ENTRYPOINT ["/usr/local/bin/appserver"]

FROM runtime AS client
COPY --from=build /src/target/release/client /usr/local/bin/client
ENTRYPOINT ["/usr/local/bin/client"]

FROM client AS controlplane
COPY --from=build /src/target/release/controlplane /usr/local/bin/controlplane
COPY --from=frontend /src/dist /lab/controlplane/frontend/dist
ENTRYPOINT ["/usr/local/bin/controlplane"]
