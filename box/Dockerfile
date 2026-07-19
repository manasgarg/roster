# The box: a CAPABLE machine behind a governed door (docs/box.md).
# Tools are baked in deliberately — on the NAT-less network more binaries add
# no egress power, and a missing tool is an illegible "no" that belongs to the
# gateway instead. pi + the box extensions are baked at the version pinned by
# the repo's lockfile; `[engine] dir` in org.toml is a dev override that
# mounts a checkout over them.
#
# Published on every release as ghcr.io/manasgarg/roster-box (:latest and
# :<version>) — the host pulls it automatically. To build locally instead,
# build from the REPO ROOT (the engine layers need package-lock.json + box/):
#   docker build -t roster-box -f box/Dockerfile .
# and point org.toml at it:  [engine] image = "roster-box"
FROM node:24-bookworm-slim

# The toolbelt — one image, everything installed. Base image already carries
# node/npm/npx. build-essential is the big line (~250 MB): native pip/npm
# builds are the most common illegible failure when it is absent.
RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    bash ca-certificates curl git jq python3 ripgrep \
    build-essential pkg-config \
    ffmpeg imagemagick pandoc \
    fd-find file git-lfs less moreutils openssl poppler-utils procps \
    python3-pip python3-venv rsync sqlite3 tree tzdata unzip wget \
    xz-utils zip zstd \
  && rm -rf /var/lib/apt/lists/* \
  && ln -s "$(command -v fdfind)" /usr/local/bin/fd \
  && git lfs install --system

# gh — the official apt repo (refreshed on every image rebuild).
RUN mkdir -p -m 755 /etc/apt/keyrings \
  && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
       -o /etc/apt/keyrings/githubcli-archive-keyring.gpg \
  && chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg \
  && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
       > /etc/apt/sources.list.d/github-cli.list \
  && apt-get update \
  && apt-get install -y --no-install-recommends gh \
  && rm -rf /var/lib/apt/lists/*

# yq (mikefarah, static) and uv (fast Python package manager, static).
RUN curl -fsSL "https://github.com/mikefarah/yq/releases/latest/download/yq_linux_$(dpkg --print-architecture)" \
       -o /usr/local/bin/yq \
  && chmod +x /usr/local/bin/yq
COPY --from=ghcr.io/astral-sh/uv:latest /uv /uvx /usr/local/bin/

# roster-lock — the named-lock helper for store/ writes (flock on
# store/.locks/<name>; the host's backup pass honors the same locks).
COPY box/roster-lock /usr/local/bin/roster-lock
RUN chmod +x /usr/local/bin/roster-lock

# pnpm/yarn shims via corepack (ships with node).
RUN corepack enable

# Strip every setuid/setgid bit (su, mount, newgrp, passwd, …). The box runs as
# a non-root uid with --security-opt=no-new-privileges, so these are already
# neutralized at runtime; removing the bits is belt-and-suspenders and shrinks
# the in-container privilege-escalation surface to nothing (F2).
RUN find / -xdev -type f \( -perm -4000 -o -perm -2000 \) -exec chmod -s {} + 2>/dev/null || true

# The engine: pi + extensions at the lockfile-pinned version. /opt/roster/engine/run-pi
# is the stable entry the host invokes — it expands the baked extensions itself,
# so the host never needs to inspect the image.
COPY package.json package-lock.json /opt/roster/engine/
RUN cd /opt/roster/engine && npm ci --omit=dev && npm cache clean --force
COPY box/extensions /opt/roster/engine/box/extensions
RUN node -e "const path=require('path');const dir='/opt/roster/engine/node_modules/@earendil-works/pi-coding-agent';const p=require(dir+'/package.json');const bin=typeof p.bin==='string'?p.bin:p.bin.pi;require('fs').symlinkSync(path.join(dir,bin),'/opt/roster/engine/pi')" \
  && printf '#!/bin/sh\nexec node /opt/roster/engine/pi --no-extensions $(for f in /opt/roster/engine/box/extensions/*.ts; do printf -- "-e %%s " "$f"; done) "$@"\n' > /opt/roster/engine/run-pi \
  && chmod +x /opt/roster/engine/run-pi

WORKDIR /workspace
