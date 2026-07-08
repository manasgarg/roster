# The box: just enough to run pi headless. pi itself is NOT baked in — it
# comes from the read-only repo bind-mount, so the box always runs the exact
# pinned version on disk and cannot tamper with its own tools.
FROM node:24-bookworm-slim

RUN apt-get update \
  && apt-get install -y --no-install-recommends bash ca-certificates git ripgrep curl python3 jq \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
