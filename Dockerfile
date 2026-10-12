FROM node:22-bookworm AS ui-builder

WORKDIR /app

COPY ui/package.json ui/package-lock.json ./ui/
RUN cd ui && npm ci

COPY ui ./ui
COPY scripts/generate-assets-table.mjs ./scripts/generate-assets-table.mjs
COPY src/ui/assets.rs ./src/ui/assets.rs
RUN cd ui && npm run build && npm run check
RUN node scripts/generate-assets-table.mjs

FROM rust:1.94-bookworm AS rust-builder

WORKDIR /app

COPY . .
COPY --from=ui-builder /app/ui/dist ./ui/dist
COPY --from=ui-builder /app/src/ui/assets.rs ./src/ui/assets.rs
ENV NK_TARGET_NEON=0 \
    NK_TARGET_NEONHALF=0 \
    NK_TARGET_NEONSDOT=0 \
    NK_TARGET_NEONBFDOT=0 \
    NK_TARGET_NEONFHM=0 \
    NK_TARGET_SVE=0 \
    NK_TARGET_SVEHALF=0 \
    NK_TARGET_SVEBFDOT=0 \
    NK_TARGET_SVESDOT=0 \
    NK_TARGET_SVE2=0 \
    NK_TARGET_SVE2P1=0 \
    NK_TARGET_NEONFP8=0 \
    NK_TARGET_SME=0 \
    NK_TARGET_SME2=0 \
    NK_TARGET_SME2P1=0 \
    NK_TARGET_SMEF64=0 \
    NK_TARGET_SMEHALF=0 \
    NK_TARGET_SMEBF16=0 \
    NK_TARGET_SMEBI32=0 \
    NK_TARGET_SMELUT2=0 \
    NK_TARGET_SMEFA64=0
RUN cargo build --release --all-features --locked -p mcpmem

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates poppler-utils \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --gid 10001 mcpmem \
    && useradd --uid 10001 --gid mcpmem --no-create-home --shell /usr/sbin/nologin mcpmem \
    && install --directory --owner=mcpmem --group=mcpmem /var/lib/mcpmem \
    && install --directory /etc/mcpmem

COPY --from=rust-builder /app/target/release/mcpmem /usr/local/bin/mcpmem
COPY docker/mcpmem.toml /etc/mcpmem/mcpmem.toml
COPY --chmod=755 docker/entrypoint.sh /usr/local/bin/entrypoint.sh

USER 10001

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
