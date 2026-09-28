# syntax=docker/dockerfile:1
#
# synthlite as a static binary in an otherwise empty image.
#
#   docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/work" \
#     -e OPENAI_API_KEY -e OPENAI_BASE_URL -e OPENAI_MODEL \
#     ghcr.io/scogo-ai/synthlite prompts.jsonl        # writes ./out
#
# Secrets come in through -e only; nothing is baked into the image.

FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Alpine's Rust targets musl and links it statically by default. TLS is rustls
# with the Mozilla root store compiled in (webpki-roots), so the final image
# needs no CA bundle.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/synthlite /synthlite \
    && strip /synthlite \
    && /synthlite --version \
    && mkdir -p /rootfs/work /rootfs/tmp \
    && chown 65532:65532 /rootfs/work \
    && chmod 1777 /rootfs/tmp

FROM scratch
LABEL org.opencontainers.image.title="synthlite" \
      org.opencontainers.image.description="Prompts in, a private fine-tuning dataset out." \
      org.opencontainers.image.source="https://github.com/scogo-ai/synthlite" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.vendor="Scogo AI"
COPY --from=build /rootfs/ /
COPY --from=build /synthlite /usr/local/bin/synthlite
ENV PATH=/usr/local/bin
WORKDIR /work
USER 65532:65532
ENTRYPOINT ["synthlite"]
