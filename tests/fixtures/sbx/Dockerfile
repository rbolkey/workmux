# Thin sbx layer over a workmux agent image: sbx templates expect a non-root
# `agent` user (UID 1000, home /home/agent), a CA directory for the proxy's
# certificate, and BASH_ENV pointing at /etc/sandbox-persistent.sh.
ARG BASE=ghcr.io/raine/workmux-sandbox:claude
FROM ${BASE}
RUN useradd -m -u 1000 -s /bin/bash agent \
    && mkdir -p /usr/local/share/ca-certificates \
    && touch /etc/sandbox-persistent.sh \
    && install -d -o agent -g agent /home/agent/.claude
ENV BASH_ENV=/etc/sandbox-persistent.sh
USER agent
WORKDIR /home/agent
