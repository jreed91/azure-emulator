# The emulator as a container image: one binary, no other dependencies.
#
#   docker build -t azure-emulator .
#   docker run -p 10443:10443 -p 5672:5672 -p 5671:5671 azure-emulator
#
# Configure it with the AZURE_EMULATOR_* environment variables (see
# `azure-emulator --help`), and mount a provisioning file with
# AZURE_EMULATOR_CONFIG to create resources at startup.

FROM rust:1-slim-bookworm AS build
# The release tag (v1.2.3), reported by `azure-emulator --version`; release.yml
# passes it. Left empty, the binary reports the crate version.
ARG VERSION=
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    if [ -n "$VERSION" ]; then export AZURE_EMULATOR_VERSION="${VERSION#v}"; fi \
    && cargo build --release --locked -p azure-emulator \
    && cp target/release/azure-emulator /azure-emulator

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home-dir /data --create-home emulator
COPY --from=build /azure-emulator /usr/local/bin/azure-emulator
USER emulator
# Listen on every interface (the default is loopback only), and keep the
# generated CA certificate where a volume can share it with clients.
ENV AZURE_EMULATOR_BIND=0.0.0.0 \
    AZURE_EMULATOR_HOME=/data
# 10000-10002 are the path-style (Azurite-style) storage ports, served when
# AZURE_EMULATOR_BLOB_PORT, _QUEUE_PORT and _TABLE_PORT name them.
EXPOSE 10443 5672 5671 1433 10000 10001 10002
# `azure-emulator health` asks the running emulator for GET /__admin/health,
# reading the port and scheme from the same AZURE_EMULATOR_* variables (set
# them with -e, not as arguments, for the check to follow them).
HEALTHCHECK --interval=10s --timeout=6s --start-period=5s --retries=3 \
    CMD ["azure-emulator", "health"]
ENTRYPOINT ["azure-emulator"]
