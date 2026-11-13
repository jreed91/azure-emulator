# The emulator as a container image: one binary, no other dependencies.
#
#   docker build -t azure-emulator .
#   docker run -p 10443:10443 -p 5672:5672 -p 5671:5671 azure-emulator
#
# Configure it with the AZURE_EMULATOR_* environment variables (see
# `azure-emulator --help`), and mount a provisioning file with
# AZURE_EMULATOR_CONFIG to create resources at startup.

FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p azure-emulator \
    && cp target/release/azure-emulator /azure-emulator

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home-dir /data --create-home emulator
COPY --from=build /azure-emulator /usr/local/bin/azure-emulator
USER emulator
# Listen on every interface (the default is loopback only), and keep the
# generated CA certificate where a volume can share it with clients.
ENV AZURE_EMULATOR_BIND=0.0.0.0 \
    AZURE_EMULATOR_HOME=/data
EXPOSE 10443 5672 5671
ENTRYPOINT ["azure-emulator"]
