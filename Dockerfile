# -- Build Studio ----------------------------------------------
FROM node:24-bookworm-slim AS studio

WORKDIR /src/studio
# Studio depends on `@truespar/traverse-wasm` via a `file:../crates/...`
# link, so the wasm-bindgen JS package must be present BEFORE npm
# resolves dependencies. Copy it (along with the lockfile + manifest)
# into the right relative path first.
COPY crates/traverse-wasm/js/ /src/crates/traverse-wasm/js/
COPY studio/package.json studio/package-lock.json ./
# Use `npm install` rather than `npm ci`. `npm ci` is strict about the
# lockfile, but Studio depends on `@oxc-project/types` (transitively)
# which is `optional: true` with platform-specific `@emnapi/*` deps -
# the lockfile structure diverges between host platforms in ways that
# trip `npm ci`. `npm install` reconciles it without touching the
# lockfile committed to git.
RUN npm install --no-audit --no-fund --ignore-scripts
COPY studio/ ./
# the graph layout engine Studio imports from ../packages (its d3/graphology imports resolve to
# Studio's install, see resolve.dedupe in vite.config.ts)
COPY packages/ /src/packages/
# Server-only build: VITE_TRAVERSE_NO_WASM=1 (set in .env.server) makes
# Vite tree-shake the dynamic WASM-client import out of the bundle.
# tsconfig.server.json drops wasm-api.ts from type-check too. The
# served Studio talks to the server over HTTP only.
RUN npm run build:server
# Output: /src/crates/traverse-server/static/

# -- Build Server ---------------------------------------------
FROM rust:1.99.1-bookworm AS builder

RUN apt-get update && apt-get install -y protobuf-compiler && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY --from=studio /src/crates/traverse-server/static/ crates/traverse-server/static/

# Full release feature set - matches the Windows/Linux release binaries
# (`gql` is on by default for traverse-server). Without `ml` the
# traverse.ml.pipeline.* procedures error with "not built with ml".
RUN cargo build --release -p traverse-server --features grpc,tls,mcp,ml

# -- Runtime --------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/traverse-server /usr/local/bin/traverse-server
COPY --chmod=0644 LICENSE LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES /usr/share/doc/traverse/

ENV TRAVERSE_LISTEN=0.0.0.0:7690
ENV TRAVERSE_HTTP_LISTEN=0.0.0.0:7691
# no fixed API key: each container generates one and prints it at startup (docker logs);
# set TRAVERSE_API_KEY for a key of your own

EXPOSE 7690 7691 50051

VOLUME /data

ENTRYPOINT ["traverse-server"]
CMD ["--data", "/data"]
