FROM node:22-bookworm-slim

ARG CLAUDE_AGENT_SDK_PACKAGE=@anthropic-ai/claude-agent-sdk
ARG CLAUDE_AGENT_SDK_NATIVE_PACKAGE=@anthropic-ai/claude-agent-sdk-linux-arm64

ENV CLAUDE_CONFIG_DIR=/home/exo/.claude
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

RUN npm install -g \
        "${CLAUDE_AGENT_SDK_PACKAGE}" \
        "${CLAUDE_AGENT_SDK_NATIVE_PACKAGE}" \
    && CLAUDE_BIN="$(find "$(npm root -g)" -path "*/@anthropic-ai/claude-agent-sdk-linux-arm64/claude" -type f -print -quit)" \
    && test -n "${CLAUDE_BIN}" \
    && chmod +x "${CLAUDE_BIN}" \
    && ln -sf "${CLAUDE_BIN}" /usr/local/bin/claude-code

RUN mkdir -p /home/exo/.claude /home/exo/workspace

WORKDIR /home/exo/workspace

CMD ["bash"]
