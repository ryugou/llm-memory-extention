#!/bin/bash
# GCE instance startup script for llm-memory-extension.
# Assumes:
#   - Container-Optimized OS or Ubuntu LTS with docker + docker-compose installed
#   - /opt/llm-memory contains docker-compose.yml, .env, and litestream.yml
#   - /var/lib/llm-memory exists (or will be created) for persistent SQLite
set -euo pipefail

cd /opt/llm-memory
docker compose pull
docker compose up -d
docker image prune -af --filter "until=168h"
