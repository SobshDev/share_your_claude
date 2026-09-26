# Operations runbook

Copy-paste procedures for running Shared Router under Docker Compose, including Dokploy. Run every command on the Docker host as a user who can use `docker`. Commands use Bash.

Keep three things safe and separate: the database backups, the `ENCRYPTION_KEY` value that was active when each backup was taken, and the owner password. A database backup without its matching key cannot recover the saved Claude login.

## Find the container and volume

Compose prefixes names with the project name, so the `router_data` volume appears on the host as `<project>_router_data`. On Dokploy the project name is the service's app name, and the Compose directory is managed by Dokploy. These commands find the names from Docker labels, so they work from any directory:

```bash
router_container=$(docker ps -a --filter label=com.docker.compose.service=router --format '{{.Names}}')
echo "$router_container"
router_volume=$(docker inspect "$router_container" --format '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Name}}{{end}}{{end}}')
echo "$router_volume"
```

Each command must print exactly one name. If several Compose projects define a service called `router`, add `--filter label=com.docker.compose.project=<project>` to the first command. `docker compose ls` and `docker volume ls | grep router_data` list the candidates.

The runtime image contains only the router binary, `ca-certificates`, and the license notices in `/usr/share/doc/shared-router/`. It has no `curl` and no SQLite shell, so readiness checks below use the binary's `healthcheck` command, and offline database work uses a short-lived `debian:bookworm-slim` container with the volume mounted. The router runs as UID 10001, and every file it opens must belong to that user.

## Back up

A backup uses SQLite `VACUUM INTO` inside the running container, so the router can keep serving. Never copy the live `router.sqlite` alone: with WAL mode, recent writes live in `router.sqlite-wal`.

```bash
umask 077
router_backup="router-$(date -u +%Y%m%dT%H%M%SZ).sqlite"
docker exec "$router_container" shared-router backup "/data/$router_backup"
docker cp "$router_container:/data/$router_backup" "./$router_backup"
docker exec "$router_container" rm "/data/$router_backup"
chmod 600 "./$router_backup"
```

From a Compose directory that uses the same project name, `bash scripts/backup.sh` does the same, writes to `./backups` or to `BACKUP_DIR`, and with `BACKUP_KEEP=N` keeps only the newest N backups there. On Dokploy, point `BACKUP_DIR` outside the checkout. Move the file to protected storage. Backups contain hashed keys, usage history, and the encrypted Claude tokens, so treat them as private.

## Restore

1. Stop the router so nothing writes to the volume:

   ```bash
   docker stop "$router_container"
   ```

2. Replace the database with `scripts/restore.sh`, from a checkout of this repository on the Docker host (any directory works):

   ```bash
   BACKUP_DIR=/srv/router-backups bash scripts/restore.sh /srv/router-backups/router-20260101T000000Z.sqlite
   ```

   The script finds the router container and its `/data` volume from the Compose labels (set `ROUTER_PROJECT=<project>` when several projects define a `router` service) and refuses to run while the router is up; `--force` stops it for you. Before replacing anything, it copies the current database, and its WAL file if present, to `BACKUP_DIR` (default `./backups`) as `pre-restore-<timestamp>.sqlite`, so a restore of the wrong file can be undone the same way. It then deletes the old WAL and SHM files, which belong to the database being replaced and which SQLite would otherwise apply to the restored file, and installs the backup owned by UID 10001 with mode 600. It leaves the router stopped. The helper containers use `debian:bookworm-slim` (override with `HELPER_IMAGE`), which Docker pulls on first use.

   Without a checkout, the equivalent manual step is the following, run from the directory that holds the backup, with `router_backup` set to its file name. It skips the pre-restore copy:

   ```bash
   docker run --rm -v "$router_volume":/data -v "$PWD":/in:ro -e router_backup="$router_backup" debian:bookworm-slim \
     sh -c 'rm -f /data/router.sqlite-wal /data/router.sqlite-shm && cp "/in/$router_backup" /data/router.sqlite && chown 10001:10001 /data/router.sqlite && chmod 600 /data/router.sqlite'
   ```

3. Make sure the deployment's `ENCRYPTION_KEY` is the key that was active when the backup was taken. If you changed it in Dokploy, restore the old value and redeploy instead of starting the container directly.
4. Start the router and check readiness:

   ```bash
   docker start "$router_container"
   docker exec "$router_container" shared-router healthcheck && echo ready
   ```

   The last command prints `ready`. Sign in to the dashboard and check that **Friends & keys** shows the people and keys from the backup.

Startup marks requests that were in progress at backup time as `interrupted` and removes expired sessions. If the backup's refresh token has already been rotated, the dashboard will ask you to connect Claude again. A key issued after the backup no longer exists and must be issued again.

## Upgrade

Migrations are embedded in the binary. They run automatically every time the router starts, before it begins serving, and they are forward-only: there are no down migrations. An older image will not start against a database that a newer image has migrated, because the database records migrations the older binary does not know.

1. Take a backup as above and keep it with the image or tag you are currently running.
2. Read [CHANGELOG.md](../CHANGELOG.md) for configuration changes and new migrations.
3. Deploy the new version (redeploy in Dokploy, or `docker compose up -d --build`).
4. Check `/readyz` and the dashboard.

To roll back, redeploy the previous version and restore the backup from step 1 using the restore procedure. Usage recorded since the upgrade is lost.

## Rotate the encryption key

`ENCRYPTION_KEY` encrypts only the stored Claude OAuth tokens. The router has no re-encryption command, so rotation means discarding the saved Claude login and connecting again under the new key.

1. Take a backup, and store the current key with it.
2. Generate a new key:

   ```bash
   docker exec "$router_container" shared-router generate-key
   ```

3. Replace `ENCRYPTION_KEY` in Dokploy's **Environment** settings (or `.env`) and redeploy.
4. Sign in to the dashboard, open **Claude connection**, and click **Connect Claude**. Completing the login replaces the stored tokens, now encrypted with the new key.

Between steps 3 and 4, friend requests fail with 500 "The router could not complete this operation", because the old ciphertext cannot be decrypted. The dashboard can still show the connection as connected during that window. If you are rotating because the key and a database copy may have leaked together, treat the stored Claude tokens as exposed too and sign out other sessions from your Claude account.

## Rotate the owner password

1. Generate a hash for the new password without putting it in shell history or process arguments:

   ```bash
   read -r -s -p 'New owner password: ' router_password; echo
   printf '%s' "$router_password" | docker exec -i "$router_container" shared-router hash-password
   unset router_password
   ```

   Do not use `scripts/bootstrap-secrets.sh` for this: it also prints a new encryption key.
2. Replace `ADMIN_PASSWORD_HASH` in Dokploy's **Environment** settings, keeping the single quotes, and redeploy.
3. Invalidate existing dashboard sessions. They are stored in the database and are not tied to the password hash, so without this step a session opened with the old password stays valid for up to 12 hours, even after a restart. Delete all sessions while the router is stopped:

   ```bash
   docker stop "$router_container"
   docker run --rm -v "$router_volume":/data debian:bookworm-slim sh -c \
     'apt-get update -qq && apt-get install -y -qq --no-install-recommends sqlite3 >/dev/null && sqlite3 /data/router.sqlite "DELETE FROM admin_session;" && chown 10001:10001 /data/router.sqlite*'
   docker start "$router_container"
   ```

   Every browser, including yours, must sign in again. The helper container needs network access to install `sqlite3`.

## Troubleshooting

Router logs never contain prompts, tokens, or SQL values. Read them with `docker logs "$router_container"` (or `docker compose logs router`).

**Claude needs reconnection (`needs_reauth`).** Friends get 503 with "The owner must reconnect Claude in the dashboard". The router sets this state when a token refresh is rejected with 400, 401, or 403, and when Claude answers a proxied request with 401 or 403. It never falls back to other billing. Sign in, open **Claude connection**, and click **Connect Claude**. Restoring an older backup can cause this too, because its refresh token may already have been used.

**Every friend request fails with 500 while the dashboard shows connected.** The stored tokens cannot be decrypted, almost always because `ENCRYPTION_KEY` changed. Restore the original key and redeploy, or connect Claude again to re-encrypt with the current key.

**Sign-in returns 429 "Too many sign-in attempts".** The service allows five sign-in attempts per minute in total, from every source combined. Wait one minute and try again. Restarting the router also clears the counter. Repeated lockouts that you did not cause mean someone else is sending sign-in attempts to the dashboard.

**Sign-in or dashboard changes return 403 "This action must originate from the owner dashboard".** The browser origin differs from `PUBLIC_ORIGIN`. They must match exactly, including scheme and port.

**Friends get 429 "The router is busy".** At most 8 upstream requests run at once across all keys, and extra requests are refused immediately rather than queued. Retry after running requests finish. A 429 that says "Claude rejected the request" comes from Claude itself, and its `retry-after` header is passed through.

**`/readyz` fails or the container is unhealthy.** `/readyz` runs one SQLite query. If nothing answers at all, the process did not start: check the logs for the first error. Common causes:

- `PUBLIC_ORIGIN must be set`, `ENCRYPTION_KEY must contain 32 bytes encoded as base64`, `invalid admin password hash`, or `admin password must use Argon2id`: fix the environment value. The hash needs single quotes in Dokploy and `.env` so its `$` characters stay literal.
- A permission or "unable to open database file" error: the files in the volume are not owned by UID 10001. Run the restore step 2 `chown` again.
- A migration error after a downgrade: the database was migrated by a newer version. Deploy that version again, or restore a backup taken before the upgrade.

If the process answers `/healthz` but not `/readyz`, the database is unavailable. Check the volume and disk space.

**The OAuth callback is rejected.**

- "This connection expired or belongs to another session. Start again": more than ten minutes passed, you signed in again, or you clicked **Connect Claude** again. Only the most recent link works.
- "Start a new Claude connection first": the router restarted after you clicked **Connect Claude**. Pending logins are kept in memory.
- "Unexpected OAuth redirect URL": paste the whole address unchanged, starting with `http://localhost:54545/callback`. Replacing `localhost` with `127.0.0.1` does not work.

In every case, click **Connect Claude** and complete the new link within ten minutes.
