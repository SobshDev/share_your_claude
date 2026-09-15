#!/usr/bin/env bash
set -euo pipefail
umask 077
mkdir -p secrets
if [[ -e secrets/encryption_key || -e secrets/admin_password_hash ]]; then
  echo 'Secrets already exist; refusing to overwrite them.' >&2
  exit 1
fi
read -r -s -p 'Owner password (at least 12 characters): ' router_password
echo
read -r -s -p 'Repeat password: ' router_confirmation
echo
if [[ "$router_password" != "$router_confirmation" ]]; then
  echo 'Passwords did not match.' >&2
  exit 1
fi
# The short-lived container reads the password from stdin, never command arguments.
printf '%s' "$router_password" | docker run --rm -i shared-router:local hash-password > secrets/admin_password_hash
unset router_password router_confirmation
docker run --rm shared-router:local generate-key > secrets/encryption_key
# Compose file secrets are bind mounts and keep host permissions. The image runs as UID 10001.
# Keep the enclosing directory owner-only; make only the encrypted-key input files readable inside it.
chmod 700 secrets
chmod 644 secrets/encryption_key secrets/admin_password_hash
echo 'Secrets created. Copy their contents into Dokploy File Mounts named encryption_key and admin_password_hash.'
echo 'Keep a separate, protected backup of the encryption key.'
