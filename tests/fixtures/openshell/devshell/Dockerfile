# Bakes the devshell's closure into a sandbox image, built entirely on the
# workstation: nothing in the sandbox evaluates or builds. The builder stage
# realises the devshell, records its environment as a sourceable script and
# copies every store path the environment needs; the sandbox image only gets
# /nix/store plus the script, which /usr/local/bin/devshell sources before
# exec'ing its arguments (no `nix develop`).
ARG BASE=wm-learn-openshell:latest

FROM docker.io/nixos/nix:2.24.10 AS build
WORKDIR /devshell
COPY flake.nix flake.lock ./
RUN nix --extra-experimental-features "nix-command flakes" develop --profile /devshell/profile -c true \
    && nix --extra-experimental-features "nix-command flakes" print-dev-env /devshell/profile > /devshell/env.sh \
    && mkdir /closure \
    && cp -a $(nix-store -qR /devshell/profile) /closure/

FROM ${BASE}
COPY --from=build /closure /nix/store
COPY --from=build /devshell/env.sh /nix/devshell-env.sh
COPY devshell /usr/local/bin/devshell
