#!/usr/bin/env bash
# Run from the deployed Compose directory.
#   BACKUP_DIR   destination directory (default: ./backups); keep it outside the deploy checkout
#   BACKUP_KEEP  keep only the newest N router-*.sqlite files in BACKUP_DIR (default: keep all)
set -euo pipefail
umask 077
backup_dir="${BACKUP_DIR:-$PWD/backups}"
backup_keep="${BACKUP_KEEP:-}"
if [[ -n "$backup_keep" && ! "$backup_keep" =~ ^[1-9][0-9]*$ ]]; then
  echo 'BACKUP_KEEP must be a positive integer.' >&2
  exit 1
fi
mkdir -p "$backup_dir"
backup_dir=$(cd "$backup_dir" && pwd -P)
router_backup="router-$(date -u +%Y%m%dT%H%M%SZ).sqlite"
backup_file="$backup_dir/$router_backup"
backup_saved=0
cleanup() {
  # The snapshot in /data is a full plaintext copy; never leave it next to live data.
  docker compose exec -T router rm -f "/data/$router_backup" >/dev/null 2>&1 || true
  if [[ "$backup_saved" != 1 ]]; then
    rm -f "$backup_file"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
docker compose exec -T router shared-router backup "/data/$router_backup"
docker compose cp "router:/data/$router_backup" "$backup_file"
chmod 600 "$backup_file"
backup_saved=1
if command -v sqlite3 >/dev/null 2>&1; then
  integrity=$(sqlite3 -readonly "$backup_file" 'PRAGMA integrity_check;' 2>&1 || true)
  if [[ "$integrity" != ok ]]; then
    echo "Integrity check failed for $backup_file; keeping it and skipping pruning:" >&2
    printf '%s\n' "$integrity" >&2
    exit 1
  fi
else
  echo 'Warning: sqlite3 not found; skipped the backup integrity check.' >&2
fi
echo "Saved $backup_file"
if [[ -n "$backup_keep" ]]; then
  shopt -s nullglob
  backups=("$backup_dir"/router-*.sqlite)
  if (( ${#backups[@]} > backup_keep )); then
    for old_backup in "${backups[@]:0:${#backups[@]}-backup_keep}"; do
      rm -f -- "$old_backup"
      echo "Pruned $old_backup"
    done
  fi
fi
