# syntax=docker/dockerfile:1

# SPDX-FileCopyrightText: 2026 Sigmastone OÜ
# SPDX-License-Identifier: Apache-2.0

# The server as a container image: one static-enough binary on a base with nothing else in it.
#
#   docker build -t epistel .
#   docker run -p 993:993 -p 465:465 -v /etc/epistel:/etc/epistel:ro epistel
#
# The configuration is expected at /etc/epistel/epistel.toml, with the certificate and key
# wherever it says. The process runs as an unprivileged user (uid 65532), so those files have to
# be readable by it.

FROM rust:1-bookworm AS build
# The TLS library under the cloud SDKs builds C code.
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p epistel && cp target/release/epistel /epistel

# glibc, CA certificates and time zones; no shell, no package manager.
FROM gcr.io/distroless/cc-debian12:nonroot
LABEL org.opencontainers.image.title="Epistel" \
      org.opencontainers.image.description="Mail server that serves mail of domains on Amazon SES to standard clients over IMAP and SMTP Submission." \
      org.opencontainers.image.vendor="Sigmastone OÜ" \
      org.opencontainers.image.url="https://sigmastone.io" \
      org.opencontainers.image.source="https://github.com/sigmastone-io/epistel" \
      org.opencontainers.image.licenses="Apache-2.0"
COPY --from=build /epistel /usr/local/bin/epistel
COPY LICENSE NOTICE /usr/share/doc/epistel/
USER nonroot
EXPOSE 993 465
ENTRYPOINT ["/usr/local/bin/epistel"]
CMD ["serve"]
