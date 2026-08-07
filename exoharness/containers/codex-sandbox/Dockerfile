FROM node:22-bookworm-slim

ARG CODEX_PACKAGE=@openai/codex

ENV CODEX_HOME=/home/exo/.codex
ENV HOME=/home/exo
ENV PATH=/usr/local/bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        git \
        openssh-client \
        ripgrep \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g "${CODEX_PACKAGE}"

RUN mkdir -p /home/exo/.codex /home/exo/workspace

WORKDIR /home/exo/workspace

CMD ["bash"]
