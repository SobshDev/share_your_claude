#!/usr/bin/env bash
set -euo pipefail
router_image="${ROUTER_IMAGE:-shared-router:local}"
if ! docker image inspect "$router_image" >/dev/null 2>&1; then
  echo "Image $router_image not found. Build the image first: docker build -t $router_image ." >&2
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
router_hash=$(printf '%s' "$router_password" | docker run --rm -i --pull never --network none "$router_image" hash-password)
unset router_password router_confirmation
router_key=$(docker run --rm --pull never --network none "$router_image" generate-key)
echo 'Paste these values into Dokploy Environment settings (keep the quotes):' >&2
printf "ENCRYPTION_KEY='%s'\nADMIN_PASSWORD_HASH='%s'\n" "$router_key" "$router_hash"
unset router_key router_hash
echo 'Keep the encryption key stable across redeployments and back it up securely.' >&2
