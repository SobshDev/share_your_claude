#!/usr/bin/env bash
# Restores a router database backup into the Compose volume. Run on the Docker host:
#   bash scripts/restore.sh [--force] BACKUP_FILE
#
# The router must be stopped first (docker stop <container>); --force stops it for you.
# Before replacing anything, the current database is copied to BACKUP_DIR as
# pre-restore-<UTC timestamp>.sqlite (plus its -wal file when present).
#   BACKUP_DIR      where the pre-restore copy goes (default: ./backups)
#   ROUTER_PROJECT  Compose project name, when several projects define a "router" service
#   HELPER_IMAGE    image for the short-lived file-copy containers (default: debian:bookworm-slim)
# The router is left stopped: check ENCRYPTION_KEY, then start it and run
# "shared-router healthcheck" inside it (see docs/operations.md).
set -euo pipefail
umask 077

usage() {
  echo 'Usage: bash scripts/restore.sh [--force] BACKUP_FILE' >&2
  exit 2
}
force=0
backup_file=''
for arg in "$@"; do
  case "$arg" in
    --force) force=1 ;;
    -*) usage ;;
    *)
      [[ -z "$backup_file" ]] || usage
      backup_file="$arg"
      ;;
  esac
done
[[ -n "$backup_file" ]] || usage

if [[ ! -f "$backup_file" || ! -r "$backup_file" ]]; then
  echo "Cannot read $backup_file." >&2
  exit 1
fi
if [[ "$(head -c 15 -- "$backup_file")" != 'SQLite format 3' ]]; then
  echo "$backup_file is not a SQLite database." >&2
  exit 1
fi
backup_file="$(cd "$(dirname -- "$backup_file")" && pwd -P)/$(basename -- "$backup_file")"
helper_image="${HELPER_IMAGE:-debian:bookworm-slim}"
backup_dir="${BACKUP_DIR:-$PWD/backups}"
mkdir -p "$backup_dir"
backup_dir=$(cd "$backup_dir" && pwd -P)

filters=(--filter label=com.docker.compose.service=router)
if [[ -n "${ROUTER_PROJECT:-}" ]]; then
  filters+=(--filter "label=com.docker.compose.project=$ROUTER_PROJECT")
fi
containers=()
while IFS= read -r name; do
  [[ -n "$name" ]] && containers+=("$name")
done < <(docker ps -a "${filters[@]}" --format '{{.Names}}')
if (( ${#containers[@]} != 1 )); then
  echo "Expected one router container, found ${#containers[@]}: ${containers[*]:-none}." >&2
  echo 'Set ROUTER_PROJECT to the Compose project name (see docker compose ls).' >&2
  exit 1
fi
router_container=${containers[0]}
router_volume=$(docker inspect "$router_container" \
  --format '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Name}}{{end}}{{end}}')
if [[ -z "$router_volume" ]]; then
  echo "$router_container has no volume mounted at /data." >&2
  exit 1
fi
echo "Router container: $router_container"
echo "Data volume: $router_volume"

if [[ "$(docker inspect "$router_container" --format '{{.State.Running}}')" == true ]]; then
  if (( ! force )); then
    echo "$router_container is running. Stop it first (docker stop $router_container), or pass --force." >&2
    exit 1
  fi
  echo "Stopping $router_container"
  docker stop "$router_container" >/dev/null
fi

# The router is stopped, so the database file and its WAL form a consistent pair.
pre_restore="pre-restore-$(date -u +%Y%m%dT%H%M%SZ).sqlite"
docker run --rm --network none -v "$router_volume":/data:ro -v "$backup_dir":/out \
  -e pre_restore="$pre_restore" -e owner="$(id -u):$(id -g)" "$helper_image" sh -euc '
    if [ ! -f /data/router.sqlite ]; then
      echo "No existing database to save."
      exit 0
    fi
    for suffix in "" -wal; do
      if [ -f "/data/router.sqlite$suffix" ]; then
        cp "/data/router.sqlite$suffix" "/out/$pre_restore$suffix"
        chown "$owner" "/out/$pre_restore$suffix"
        chmod 600 "/out/$pre_restore$suffix"
      fi
    done'
if [[ -f "$backup_dir/$pre_restore" ]]; then
  echo "Saved the current database as $backup_dir/$pre_restore"
fi

# Leftover -wal and -shm files belong to the database being replaced, and SQLite would apply
# them to the restored file, so they are removed before the new file is moved into place.
docker run --rm --network none -v "$router_volume":/data -v "$backup_file":/in/router.sqlite:ro \
  "$helper_image" sh -euc '
    cp /in/router.sqlite /data/.router-restore.tmp
    chown 10001:10001 /data/.router-restore.tmp
    chmod 600 /data/.router-restore.tmp
    rm -f /data/router.sqlite-wal /data/router.sqlite-shm
    mv /data/.router-restore.tmp /data/router.sqlite'
echo "Restored $backup_file into $router_volume."
echo 'Check that ENCRYPTION_KEY is the key that was active when the backup was taken, then run:'
echo "  docker start $router_container"
echo "  docker exec $router_container shared-router healthcheck && echo ready"
