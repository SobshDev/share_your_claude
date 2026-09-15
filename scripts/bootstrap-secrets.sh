#!/usr/bin/env bash
set -euo pipefail
read -r -s -p 'Owner password (at least 12 characters): ' router_password
echo
read -r -s -p 'Repeat password: ' router_confirmation
echo
if [[ "$router_password" != "$router_confirmation" ]]; then
  echo 'Passwords did not match.' >&2
  exit 1
fi
# The short-lived container reads the password from stdin, never command arguments.
router_hash=$(printf '%s' "$router_password" | docker run --rm -i shared-router:local hash-password)
unset router_password router_confirmation
router_key=$(docker run --rm shared-router:local generate-key)
echo 'Paste these values into Dokploy Environment settings (keep the quotes):' >&2
printf "ENCRYPTION_KEY='%s'\nADMIN_PASSWORD_HASH='%s'\n" "$router_key" "$router_hash"
unset router_key router_hash
echo 'Keep the encryption key stable across redeployments and back it up securely.' >&2
