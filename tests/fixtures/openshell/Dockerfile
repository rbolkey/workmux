# Image for the OpenShell learning tests: the stock image lacks git and python3,
# and a sandbox user cannot install packages, so tools must be baked in. /nix is
# pre-created for the sandbox user because it cannot create it (no root, no sudo).
FROM ubuntu:24.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends git python3 curl ca-certificates xz-utils dtach \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -m 0755 /nix && chown ubuntu:ubuntu /nix
USER ubuntu
