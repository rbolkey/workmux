# The learning image with GitHub's `gh` at /usr/bin/gh, one of the binaries
# workmux's `workmux-github` profile lets reach GitHub.
ARG BASE=wm-learn-openshell:latest

FROM ${BASE}
USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*
USER ubuntu
