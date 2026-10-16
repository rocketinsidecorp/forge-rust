#!/usr/bin/env bash
# End-to-end check of forge new. With Docker available this exercises the
# Phase 2 PostgreSQL path: reversible migrations, host serving, Compose startup,
# health and graceful shutdown. FORGE_E2E_SKIP_DOCKER=1 keeps a database-free
# host-only smoke path for environments without Docker.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/forge-e2e.XXXXXX")"
skip_docker="${FORGE_E2E_SKIP_DOCKER:-0}"
unset FORGE_E2E_SKIP_DOCKER
app_name="forge-e2e-app"
app_dir="$work_dir/$app_name"
server_pid=""

log() { printf '\n==> %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

cleanup() {
  if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
    kill -KILL "$server_pid" 2>/dev/null || true
  fi
  if [[ -f "$app_dir/compose.yaml" ]] && command -v docker >/dev/null 2>&1; then
    (cd "$app_dir" && docker compose down --volumes --remove-orphans >/dev/null 2>&1) || true
  fi
  rm -rf "$work_dir"
}
trap cleanup EXIT

free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

wait_for_http() {
  local url="$1"
  for _ in $(seq 1 100); do
    curl --silent --fail --max-time 1 "$url" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  fail "timed out waiting for $url"
}

log "Building the forge CLI"
cargo build --quiet --locked --manifest-path "$repo_root/Cargo.toml" -p forge-cli
forge="$repo_root/target/debug/forge"

log "Generating $app_name"
new_args=(new "$app_name" --skip-lockfile --forge-path .forge/crates/forge)
if [[ "$skip_docker" == 1 ]]; then
  new_args+=(--database none)
fi
(cd "$work_dir" && "$forge" "${new_args[@]}")
mkdir -p "$app_dir/.forge"
tar -C "$repo_root" --exclude ./target --exclude ./.git --exclude './examples/*/target' -cf - . \
  | tar -C "$app_dir/.forge" -xf -

cd "$app_dir"
cargo generate-lockfile --quiet

log "Running the generated application's quality gates"
cargo fmt --all -- --check
cargo clippy --quiet --all-targets --locked -- -D warnings
cargo test --quiet --locked

runtime_database_url=""
migration_database_url=""
if [[ "$skip_docker" != 1 ]]; then
  log "Starting PostgreSQL and exercising reversible migrations"
  postgres_port="$(free_port)"
  printf 'POSTGRES_PUBLISHED_PORT=%s\n' "$postgres_port" > .env
  "$forge" generate migration create_e2e_probe >/dev/null
  up_file="$(find migrations -maxdepth 1 -name '*_create_e2e_probe.up.sql' -print -quit)"
  down_file="$(find migrations -maxdepth 1 -name '*_create_e2e_probe.down.sql' -print -quit)"
  [[ -n "$up_file" && -n "$down_file" ]] || fail "migration pair was not generated"
  cat >"$up_file" <<'SQL'
CREATE TABLE forge_e2e_probe (
  id bigint PRIMARY KEY
);

CREATE TABLE forge_e2e_tenant_probe (
  id bigint PRIMARY KEY,
  tenant_id uuid NOT NULL,
  value text NOT NULL
);

ALTER TABLE forge_e2e_tenant_probe ENABLE ROW LEVEL SECURITY;
ALTER TABLE forge_e2e_tenant_probe FORCE ROW LEVEL SECURITY;

CREATE POLICY forge_e2e_tenant_isolation
ON forge_e2e_tenant_probe
FOR ALL
TO app_runtime
USING (
  tenant_id = NULLIF(current_setting('forge.tenant_id', true), '')::uuid
)
WITH CHECK (
  tenant_id = NULLIF(current_setting('forge.tenant_id', true), '')::uuid
);
SQL
  cat >"$down_file" <<'SQL'
DROP TABLE forge_e2e_tenant_probe;
DROP TABLE forge_e2e_probe;
SQL

  # sqlx::migrate!() embeds migrations at compile time, so rebuild after creating it.
  cargo build --quiet --locked
  docker compose up --detach --wait db >/dev/null
  runtime_database_url="postgres://app_runtime:app_runtime@127.0.0.1:${postgres_port}/forge_e2e_app"
  migration_database_url="postgres://app_migrator:app_migrator@127.0.0.1:${postgres_port}/forge_e2e_app"

  if FORGE_DATABASE_URL="$runtime_database_url" "target/debug/$app_name" migrate 2>/dev/null; then
    fail "migrate must not accept the runtime database credential"
  fi

  FORGE_MIGRATION_DATABASE_URL="$migration_database_url" "target/debug/$app_name" migrate
  [[ "$(docker compose exec -T db psql -U postgres -d forge_e2e_app -Atc "SELECT to_regclass('public.forge_e2e_probe')")" == "forge_e2e_probe" ]]     || fail "migrate did not create the probe table"
  [[ "$(docker compose exec -T db psql -U postgres -d forge_e2e_app -Atc "SELECT to_regclass('public.forge_audit_events')")" == "forge_audit_events" ]] \
    || fail "framework audit migration did not create the audit table"

  audit_id="01941f29-7c00-7000-8000-000000000010"
  docker compose exec -T db psql -q -U app_runtime -d forge_e2e_app -v ON_ERROR_STOP=1 -c "
    INSERT INTO forge_audit_events (
      id, occurred_at_unix, actor_kind, action, outcome, request_link
    ) VALUES (
      '$audit_id', 42, 'system', 'e2e.audit', 'succeeded', 'e2e-request'
    );
  " >/dev/null

  [[ "$(docker compose exec -T db psql -U postgres -d forge_e2e_app -Atc \
      "SELECT count(*) FROM forge_audit_events WHERE id = '$audit_id'")" == 1 ]] \
    || fail "runtime audit append was not persisted"

  if docker compose exec -T db psql -q -U app_runtime -d forge_e2e_app \
      -v ON_ERROR_STOP=1 -c 'SELECT * FROM forge_audit_events;' >/dev/null 2>&1; then
    fail "runtime audit writer must not be able to read audit events"
  fi

  if docker compose exec -T db psql -q -U app_runtime -d forge_e2e_app \
      -v ON_ERROR_STOP=1 -c "UPDATE forge_audit_events SET outcome = 'failed' WHERE id = '$audit_id';" \
      >/dev/null 2>&1; then
    fail "runtime audit writer must not be able to update audit events"
  fi

  if docker compose exec -T db psql -q -U app_runtime -d forge_e2e_app \
      -v ON_ERROR_STOP=1 -c "DELETE FROM forge_audit_events WHERE id = '$audit_id';" \
      >/dev/null 2>&1; then
    fail "runtime audit writer must not be able to delete audit events"
  fi

  if docker compose exec -T db psql -U app_runtime -d forge_e2e_app \
      -v ON_ERROR_STOP=1 \
      -c 'CREATE TABLE forge_runtime_must_not_create_schema (id bigint);' \
      >/dev/null 2>&1; then
    fail "runtime database role must not be able to create schema objects"
  fi

  tenant_one="01941f29-7c00-7000-8000-000000000001"
  tenant_two="01941f29-7c00-7000-8000-000000000002"
  principal="01941f29-7c00-7000-8000-000000000003"
  session_id="01941f29-7c00-7000-8000-000000000020"

  [[ "$(docker compose exec -T db psql -U postgres -d forge_e2e_app -Atc \
      "SELECT to_regclass('public.forge_principals')")" == "forge_principals" ]] \
    || fail "framework identity migration did not create principals"
  [[ "$(docker compose exec -T db psql -U postgres -d forge_e2e_app -Atc \
      "SELECT to_regclass('public.forge_sessions')")" == "forge_sessions" ]] \
    || fail "framework identity migration did not create sessions"
  [[ "$(docker compose exec -T db psql -U postgres -d forge_e2e_app -Atc \
      "SELECT to_regclass('public.forge_tenant_memberships')")" == "forge_tenant_memberships" ]] \
    || fail "framework identity migration did not create memberships"

  docker compose exec -T db psql -q -U postgres -d forge_e2e_app -v ON_ERROR_STOP=1 -c "
    INSERT INTO forge_principals (id, login, password_hash)
    VALUES (
      '$principal',
      'e2e@example.invalid',
      '\$argon2id\$v=19\$m=19456,t=2,p=1\$Zm9yZ2VzYWx0Zm9yZ2VzYWx0\$Zm9yZ2VoYXNoZm9yZ2VoYXNoZm9yZ2VoYXNoZm9yZ2U'
    );
  " >/dev/null

  [[ "$(docker compose exec -T db psql -U app_runtime -d forge_e2e_app -Atc \
      "SELECT count(*) FROM forge_principals WHERE id = '$principal'")" == 1 ]] \
    || fail "runtime credential lookup cannot read principal"

  upgraded_version="$(docker compose exec -T db psql -U app_runtime -d forge_e2e_app -qAtc "
    UPDATE forge_principals
    SET password_hash = password_hash,
        updated_at = clock_timestamp(),
        version = version + 1
    WHERE id = '$principal' AND version = 1 AND disabled_at IS NULL
    RETURNING version;
  ")"
  if [[ "$upgraded_version" != 2 ]]; then
    principal_version="$(docker compose exec -T db psql -U app_runtime -d forge_e2e_app -Atc \
      "SELECT version FROM forge_principals WHERE id = '$principal'")"
    fail "runtime password rehash update did not advance optimistic version (returned '$upgraded_version'; stored version '$principal_version')"
  fi

  stale_upgrade="$(docker compose exec -T db psql -U app_runtime -d forge_e2e_app -qAtc "
    UPDATE forge_principals
    SET password_hash = password_hash,
        updated_at = clock_timestamp(),
        version = version + 1
    WHERE id = '$principal' AND version = 1 AND disabled_at IS NULL
    RETURNING version;
  ")"
  [[ -z "$stale_upgrade" ]] \
    || fail "stale password rehash version must not overwrite a newer credential"

  if docker compose exec -T db psql -q -U app_runtime -d forge_e2e_app \
      -v ON_ERROR_STOP=1 -c "
        UPDATE forge_principals
        SET login = 'mutated@example.invalid'
        WHERE id = '$principal';
      " >/dev/null 2>&1; then
    fail "runtime password rehash privilege must not allow login mutation"
  fi

  docker compose exec -T db psql -q -U app_runtime -d forge_e2e_app -v ON_ERROR_STOP=1 -c "
    INSERT INTO forge_sessions (
      id, principal_id, credential_digest, csrf_digest, issued_at, expires_at
    ) VALUES (
      '$session_id',
      '$principal',
      decode(repeat('ab', 32), 'hex'),
      decode(repeat('cd', 32), 'hex'),
      now() - interval '1 minute',
      now() + interval '1 hour'
    );
  " >/dev/null

  [[ "$(docker compose exec -T db psql -U app_runtime -d forge_e2e_app -Atc "
      SELECT count(*) FROM forge_sessions
      WHERE credential_digest = decode(repeat('ab', 32), 'hex')
        AND csrf_digest = decode(repeat('cd', 32), 'hex');
    ")" == 1 ]] \
    || fail "session bearer/CSRF digests were not persisted"

  if docker compose exec -T db psql -q -U app_runtime -d forge_e2e_app \
      -v ON_ERROR_STOP=1 -c "
        UPDATE forge_sessions
        SET issued_at = issued_at + interval '1 second'
        WHERE id = '$session_id';
      " >/dev/null 2>&1; then
    fail "persisted session lifetime must be immutable"
  fi

  docker compose exec -T db psql -q -U app_runtime -d forge_e2e_app -v ON_ERROR_STOP=1 -c "
    UPDATE forge_sessions
    SET revoked_at = now()
    WHERE id = '$session_id';
  " >/dev/null

  if docker compose exec -T db psql -q -U app_runtime -d forge_e2e_app \
      -v ON_ERROR_STOP=1 -c "
        UPDATE forge_sessions
        SET revoked_at = revoked_at + interval '1 second'
        WHERE id = '$session_id';
      " >/dev/null 2>&1; then
    fail "session revocation must be monotonic"
  fi

  if docker compose exec -T db psql -q -U app_runtime -d forge_e2e_app \
      -v ON_ERROR_STOP=1 -c "DELETE FROM forge_sessions WHERE id = '$session_id';" \
      >/dev/null 2>&1; then
    fail "runtime role must not delete session history"
  fi

  docker compose exec -T db psql -q -U app_runtime -d forge_e2e_app -v ON_ERROR_STOP=1 -c "
    INSERT INTO forge_tenant_memberships (
      tenant_id, principal_id, roles, state
    ) VALUES (
      '$tenant_one', '$principal', ARRAY['member'], 'active'
    );
    UPDATE forge_tenant_memberships
    SET roles = ARRAY['member', 'approver'], state = 'suspended'
    WHERE tenant_id = '$tenant_one' AND principal_id = '$principal';
  " >/dev/null

  [[ "$(docker compose exec -T db psql -U app_runtime -d forge_e2e_app -Atc "
      SELECT state FROM forge_tenant_memberships
      WHERE tenant_id = '$tenant_one' AND principal_id = '$principal';
    ")" == "suspended" ]] \
    || fail "membership suspension did not persist"

  docker compose exec -T db psql -q -U app_runtime -d forge_e2e_app \
      -v ON_ERROR_STOP=1 -c "
    BEGIN;
    SELECT set_config('forge.tenant_id', '$tenant_one', true);
    SELECT set_config('forge.principal_id', '$principal', true);
    INSERT INTO forge_e2e_tenant_probe (id, tenant_id, value)
    VALUES (1, '$tenant_one', 'tenant-one');
    COMMIT;
  " >/dev/null

  docker compose exec -T db psql -q -U app_runtime -d forge_e2e_app \
      -v ON_ERROR_STOP=1 -c "
    BEGIN;
    SELECT set_config('forge.tenant_id', '$tenant_two', true);
    SELECT set_config('forge.principal_id', '$principal', true);
    INSERT INTO forge_e2e_tenant_probe (id, tenant_id, value)
    VALUES (2, '$tenant_two', 'tenant-two');
    COMMIT;
  " >/dev/null

  visible_without_context="$(docker compose exec -T db psql -Atq -U app_runtime \
      -d forge_e2e_app -c "SELECT count(*) FROM forge_e2e_tenant_probe;")"
  [[ "$visible_without_context" == 0 ]] \
    || fail "RLS must fail closed without tenant context"

  visible_tenant_one="$(docker compose exec -T db psql -Atq -U app_runtime \
      -d forge_e2e_app -c "
    BEGIN;
    SELECT set_config('forge.tenant_id', '$tenant_one', true);
    SELECT set_config('forge.principal_id', '$principal', true);
    SELECT count(*) FROM forge_e2e_tenant_probe;
    ROLLBACK;
  " | tail -n 1)"
  [[ "$visible_tenant_one" == 1 ]] \
    || fail "tenant one must see exactly one tenant-scoped row"

  if docker compose exec -T db psql -q -U app_runtime -d forge_e2e_app \
      -v ON_ERROR_STOP=1 -c "
      BEGIN;
      SELECT set_config('forge.tenant_id', '$tenant_one', true);
      SELECT set_config('forge.principal_id', '$principal', true);
      INSERT INTO forge_e2e_tenant_probe (id, tenant_id, value)
      VALUES (3, '$tenant_two', 'cross-tenant');
      COMMIT;
    " >/dev/null 2>&1; then
    fail "RLS must reject a cross-tenant write"
  fi

  FORGE_MIGRATION_DATABASE_URL="$migration_database_url" "target/debug/$app_name" rollback
  [[ -z "$(docker compose exec -T db psql -U postgres -d forge_e2e_app -Atc "SELECT to_regclass('public.forge_e2e_probe')")" ]]     || fail "rollback did not remove the probe table"

  FORGE_MIGRATION_DATABASE_URL="$migration_database_url" "target/debug/$app_name" migrate
fi

log "Host run: serve, probe and graceful shutdown"
cargo build --quiet --locked
port="$(free_port)"
env_args=(FORGE_BIND="127.0.0.1:$port" FORGE_SHUTDOWN_GRACE_SECS=5)
if [[ -n "$runtime_database_url" ]]; then
  env_args+=(FORGE_DATABASE_URL="$runtime_database_url")
fi
env "${env_args[@]}" "target/debug/$app_name" &
server_pid=$!
wait_for_http "http://127.0.0.1:$port/health/live"
curl --silent --fail "http://127.0.0.1:$port/health/ready" | grep -q '"ready"' || fail "readiness body"
curl --silent --fail "http://127.0.0.1:$port/" | grep -q "\"application\":\"$app_name\"" || fail "index route"
[[ "$(curl --silent --output /dev/null --write-out '%{http_code}' "http://127.0.0.1:$port/missing")" == 404 ]]   || fail "unknown route must return 404"
env "${env_args[@]}" "target/debug/$app_name" healthcheck || fail "healthcheck subcommand"
if FORGE_BIDN="typo" "target/debug/$app_name" healthcheck 2>/dev/null; then
  fail "unknown FORGE_* keys must be rejected"
fi
kill -TERM "$server_pid"
status=0
wait "$server_pid" || status=$?
server_pid=""
[[ "$status" == 0 ]] || fail "SIGTERM should exit 0, got $status"
if env "${env_args[@]}" "target/debug/$app_name" healthcheck 2>/dev/null; then
  fail "healthcheck must fail when the server is down"
fi
echo "host run OK"

if [[ "$skip_docker" == 1 ]]; then
  log "Skipping database/container checks (FORGE_E2E_SKIP_DOCKER=1)"
  exit 0
fi

log "Building and starting the generated Compose stack"
docker compose up --detach --build >/dev/null
app_container="$(docker compose ps --quiet app)"
[[ -n "$app_container" ]] || fail "Compose app container was not created"

health=""
for _ in $(seq 1 60); do
  health="$(docker inspect --format '{{.State.Health.Status}}' "$app_container")"
  [[ "$health" == healthy ]] && break
  sleep 1
done
[[ "$health" == healthy ]] || { docker compose logs >&2; fail "container health is '$health'"; }

image="$app_name:dev"
user="$(docker image inspect --format '{{.Config.User}}' "$image")"
[[ "$user" == "65532:65532" ]] || fail "image must run as 65532:65532, got '$user'"
if docker run --rm --entrypoint /bin/sh "$image" -c true >/dev/null 2>&1; then
  fail "the runtime image must not contain a shell"
fi

curl --silent --fail "http://127.0.0.1:3000/" | grep -q "\"application\":\"$app_name\""   || fail "container index route"
docker compose logs app 2>&1 | grep -m1 -q ' | {' || fail "production logs must be JSON"

# The named volume must preserve the migrated schema across an application restart.
docker compose restart app >/dev/null
[[ "$(docker compose exec -T db psql -U postgres -d forge_e2e_app -Atc "SELECT to_regclass('public.forge_e2e_probe')")" == "forge_e2e_probe" ]]   || fail "database schema did not persist across app restart"

docker compose stop --timeout 20 app >/dev/null
app_container="$(docker compose ps --quiet --all app)"
exit_code="$(docker inspect --format '{{.State.ExitCode}}' "$app_container")"
[[ "$exit_code" == 0 ]] || { docker compose logs app >&2; fail "docker stop should exit 0, got $exit_code"; }

log "All end-to-end checks passed"
