FROM node:22-bookworm-slim

ARG PI_PACKAGE=@earendil-works/pi-coding-agent

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

# pi's bundle imports globSync from node:fs, so the base image must be node 22.
RUN npm install -g --ignore-scripts "${PI_PACKAGE}"

RUN mkdir -p /home/exo/.pi /home/exo/workspace

WORKDIR /home/exo/workspace

CMD ["bash"]
