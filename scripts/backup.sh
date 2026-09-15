#!/usr/bin/env bash
set -euo pipefail
umask 077
mkdir -p backups
router_backup="router-$(date -u +%Y%m%dT%H%M%SZ).sqlite"
docker compose exec -T router shared-router backup "/data/$router_backup"
docker compose cp "router:/data/$router_backup" "backups/$router_backup"
docker compose exec -T router rm "/data/$router_backup"
chmod 600 "backups/$router_backup"
echo "Saved backups/$router_backup"
