# Builds one image containing both the usbshare server and client.
#
#   docker build -f docker/Dockerfile -t usbshare .

FROM rust:1-slim-bookworm AS build
# Optional extra CA certificate (PEM text) for builds behind a TLS-intercepting proxy.
ARG EXTRA_CA_CERT=""
RUN if [ -n "$EXTRA_CA_CERT" ]; then \
      printf '%s\n' "$EXTRA_CA_CERT" > /usr/local/share/ca-certificates/extra.crt && update-ca-certificates; \
    fi
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/usbshare target/release/usbshare-server /usr/local/bin/

FROM debian:bookworm-slim
# Optional tools (set --build-arg INSTALL_TOOLS=0 for a minimal image):
# usbip:     stock kernel USB/IP tools, used for interoperability tests
# usbutils:  lsusb;  kmod: modprobe;  e2fsprogs/util-linux: disk tests in kernel mode
ARG INSTALL_TOOLS=1
# Use HTTPS mirrors (CA bundle taken from the build stage, which already has ca-certificates).
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
RUN if [ "$INSTALL_TOOLS" = 1 ]; then \
      sed -i 's|http://deb.debian.org|https://deb.debian.org|g' /etc/apt/sources.list.d/debian.sources \
      && apt-get update \
      && apt-get install -y --no-install-recommends usbip usbutils kmod e2fsprogs util-linux procps \
      && rm -rf /var/lib/apt/lists/*; \
    fi
COPY --from=build /usr/local/bin/usbshare /usr/local/bin/usbshare-server /usr/local/bin/
COPY docker/client-test.sh docker/kernel-test.sh /usr/local/bin/
EXPOSE 3240 8080
CMD ["usbshare-server", "--help"]
