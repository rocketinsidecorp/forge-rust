#!/usr/bin/env bash
# End-to-end check of forge new. With Docker available this exercises the
# Phase 2 PostgreSQL path: reversible migrations, host serving, Compose startup,
# health and graceful shutdown. FORGE_E2E_SKIP_DOCKER=1 keeps a database-free
# host-only smoke path for environments without Docker. Both paths serve the
# full-stack HTML page (layout, CSP, CSRF, digested Hotwire assets) and check
# the quality gates of an --api application. The Docker path also creates the
# first owner with `setup`, signs in and out through the generated pages, and
# checks the authentication tables' runtime privileges. Builds honor
# CARGO_TARGET_DIR.
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
forge="${CARGO_TARGET_DIR:-$repo_root/target}/debug/forge"
# The generated application's binary, wherever Cargo puts it.
app_bin="${CARGO_TARGET_DIR:-$app_dir/target}/debug/$app_name"

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

if [[ "$skip_docker" == 1 ]]; then
  log "Checking that forge generate authentication refuses a database-free application"
  if refusal="$("$forge" generate authentication --dry-run 2>&1)"; then
    fail "generate authentication must refuse a -d none application"
  fi
  grep -q "no database" <<<"$refusal" || fail "the refusal must explain why: $refusal"
fi

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

  if FORGE_DATABASE_URL="$runtime_database_url" "$app_bin" migrate 2>/dev/null; then
    fail "migrate must not accept the runtime database credential"
  fi

  FORGE_MIGRATION_DATABASE_URL="$migration_database_url" "$app_bin" migrate
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

  FORGE_MIGRATION_DATABASE_URL="$migration_database_url" "$app_bin" rollback
  [[ -z "$(docker compose exec -T db psql -U postgres -d forge_e2e_app -Atc "SELECT to_regclass('public.forge_e2e_probe')")" ]]     || fail "rollback did not remove the probe table"

  FORGE_MIGRATION_DATABASE_URL="$migration_database_url" "$app_bin" migrate

  log "Creating the first owner with setup"
  if docker compose exec -T db psql -q -U app_runtime -d forge_e2e_app -v ON_ERROR_STOP=1 \
      -c "INSERT INTO forge_tenants (id, slug, name) VALUES ('$tenant_two', 'intruder', 'x');" \
      >/dev/null 2>&1; then
    fail "the runtime role must not create tenants"
  fi
  if printf 'correct horse battery\n' | FORGE_DATABASE_URL="$runtime_database_url" \
      "$app_bin" setup --tenant acme --email owner@example.com --password-stdin 2>/dev/null; then
    fail "setup must not accept the runtime database credential"
  fi
  if "$app_bin" setup --tenant acme --email owner@example.com --password 'correct horse battery' \
      2>/dev/null; then
    fail "setup must refuse a password argument"
  fi
  setup_output="$(printf 'correct horse battery\n' \
    | FORGE_MIGRATION_DATABASE_URL="$migration_database_url" \
      "$app_bin" setup --tenant acme --email owner@example.com --organization "Acme Inc" --password-stdin)"
  grep -q "Created tenant acme" <<<"$setup_output" || fail "setup output: $setup_output"
  if printf 'correct horse battery\n' | FORGE_MIGRATION_DATABASE_URL="$migration_database_url" \
      "$app_bin" setup --tenant other --email other@example.com --password-stdin 2>/dev/null; then
    fail "setup must refuse once a tenant exists"
  fi
  FORGE_MIGRATION_DATABASE_URL="$migration_database_url" "$app_bin" seed | grep -q "Seed data loaded" \
    || fail "seed"
fi

log "Host run: serve, probe and graceful shutdown"
cargo build --quiet --locked
port="$(free_port)"
env_args=(FORGE_BIND="127.0.0.1:$port" FORGE_SHUTDOWN_GRACE_SECS=5)
if [[ -n "$runtime_database_url" ]]; then
  env_args+=(FORGE_DATABASE_URL="$runtime_database_url")
fi
env "${env_args[@]}" "$app_bin" &
server_pid=$!
wait_for_http "http://127.0.0.1:$port/health/live"
curl --silent --fail "http://127.0.0.1:$port/health/ready" | grep -q '"ready"' || fail "readiness body"

index_html="$work_dir/index.html"
index_headers="$work_dir/index.headers"
curl --silent --fail --dump-header "$index_headers" --output "$index_html" "http://127.0.0.1:$port/" \
  || fail "index route"
grep -q "<h1>$app_name</h1>" "$index_html" || fail "index must render the welcome view in the layout"
grep -qi '^content-type: text/html; charset=utf-8' "$index_headers" || fail "index must be HTML"
grep -qi "^content-security-policy: default-src 'self'; script-src 'self' 'sha256-" "$index_headers" \
  || fail "HTML responses must carry the strict CSP with the import map hash"
grep -q '<script type="importmap">' "$index_html" || fail "layout must render the import map"
csrf_token="$(sed -n 's/^set-cookie: __Host-forge_csrf=\([0-9a-f]\{64\}\);.*/\1/Ip' "$index_headers" | head -n 1)"
[[ -n "$csrf_token" ]] || fail "the page must issue the __Host-forge_csrf cookie"
grep -q "<meta name=\"csrf-token\" content=\"$csrf_token\">" "$index_html" \
  || fail "layout must render the CSRF meta tag with the cookie's token"
for module in turbo stimulus; do
  module_path="$(grep -o "/assets/vendor/$module-[0-9a-f]\{16\}\.js" "$index_html" | head -n 1)"
  [[ -n "$module_path" ]] || fail "import map must pin the digested $module module"
  curl --silent --fail --dump-header - --output /dev/null "http://127.0.0.1:$port$module_path" \
    | grep -qi '^content-type: text/javascript' || fail "$module must be served from the embedded assets"
done
post_status() {
  curl --silent --output /dev/null --write-out '%{http_code}' -X POST "$@" "http://127.0.0.1:$port/"
}
[[ "$(post_status --data 'name=x')" == 403 ]] || fail "a form POST without a CSRF token must be refused"
[[ "$(post_status -H "cookie: __Host-forge_csrf=$csrf_token" --data '_method=DELETE')" == 403 ]] \
  || fail "a method override without a CSRF token must be refused"
[[ "$(post_status -H "cookie: __Host-forge_csrf=$csrf_token" -H 'sec-fetch-site: cross-site' \
  --data "csrf_token=$csrf_token")" == 403 ]] || fail "a cross-site form POST must be refused"
[[ "$(post_status -H "cookie: __Host-forge_csrf=$csrf_token" --data "csrf_token=$csrf_token")" == 405 ]] \
  || fail "a form POST with the page's CSRF token must pass to routing (405: / is GET only)"
[[ "$(curl --silent --output /dev/null --write-out '%{http_code}' "http://127.0.0.1:$port/missing")" == 404 ]]   || fail "unknown route must return 404"
if [[ -n "$runtime_database_url" ]]; then
  log "Signing in and out through the generated pages"
  headers="$work_dir/auth.headers"
  # Prints the value a response's Set-Cookie gives cookie $1.
  set_cookie() { sed -n "s/^set-cookie: $1=\([0-9a-f]\{64\}\);.*/\1/Ip" "$headers" | head -n 1; }
  page="$(curl --silent --fail --dump-header "$headers" "http://127.0.0.1:$port/session/new?tenant=acme")"
  grep -q 'value="acme"' <<<"$page" || fail "the sign-in page must prefill the organization"
  anonymous="$(set_cookie __Host-forge_csrf)"
  [[ -n "$anonymous" ]] || fail "the sign-in page must issue the CSRF cookie"
  sign_in_status() {
    curl --silent --output /dev/null --dump-header "$headers" --write-out '%{http_code}' \
      -H "cookie: __Host-forge_csrf=$anonymous" \
      --data "csrf_token=$anonymous&tenant=acme&email=owner%40example.com&password=$1" \
      "http://127.0.0.1:$port/session"
  }
  [[ "$(sign_in_status wrong+password+here)" == 422 ]] || fail "a wrong password must answer 422"
  [[ "$(sign_in_status correct+horse+battery)" == 303 ]] || fail "sign-in must answer 303"
  session="$(set_cookie __Host-forge_session)"
  signed_in_csrf="$(set_cookie __Host-forge_csrf)"
  [[ -n "$session" ]] || fail "sign-in must set the session cookie"
  [[ -n "$signed_in_csrf" && "$signed_in_csrf" != "$anonymous" ]] || fail "sign-in must rotate the CSRF token"
  cookies="cookie: __Host-forge_session=$session; __Host-forge_csrf=$signed_in_csrf"
  curl --silent --fail -H "$cookies" "http://127.0.0.1:$port/session/new" | grep -q "You are signed in." \
    || fail "the sign-in page must recognize the session"
  [[ "$(curl --silent --output /dev/null --write-out '%{http_code}' -H "$cookies" \
      --data "_method=delete" "http://127.0.0.1:$port/session")" == 403 ]] \
    || fail "sign-out without the CSRF token must be refused"
  [[ "$(curl --silent --output /dev/null --write-out '%{http_code}' -H "$cookies" \
      --data "csrf_token=$signed_in_csrf&_method=delete" "http://127.0.0.1:$port/session")" == 303 ]] \
    || fail "sign-out must answer 303"
  [[ "$(docker compose exec -T db psql -U postgres -d forge_e2e_app -Atc \
      "SELECT count(*) FROM forge_sessions s JOIN forge_principals p ON p.id = s.principal_id
       WHERE p.login = 'owner@example.com' AND s.revoked_at IS NULL")" == 0 ]] \
    || fail "sign-out must revoke the session"
fi
env "${env_args[@]}" "$app_bin" healthcheck || fail "healthcheck subcommand"
if FORGE_BIDN="typo" "$app_bin" healthcheck 2>/dev/null; then
  fail "unknown FORGE_* keys must be rejected"
fi
if proxies_error="$(FORGE_HTTP_TRUSTED_PROXIES="10.0.0.0/8,0.0.0.0/0" "$app_bin" healthcheck 2>&1)"; then
  fail "an unsafe FORGE_HTTP_TRUSTED_PROXIES must be rejected"
fi
[[ "$proxies_error" == *"FORGE_HTTP_TRUSTED_PROXIES: entry 2"* && "$proxies_error" != *"0.0.0.0/0"* ]] \
  || fail "trusted proxy errors must name the entry position without echoing it"
kill -TERM "$server_pid"
status=0
wait "$server_pid" || status=$?
server_pid=""
[[ "$status" == 0 ]] || fail "SIGTERM should exit 0, got $status"
if env "${env_args[@]}" "$app_bin" healthcheck 2>/dev/null; then
  fail "healthcheck must fail when the server is down"
fi
echo "host run OK"

log "Checking an --api application's quality gates"
api_dir="$work_dir/forge-e2e-api"
(cd "$work_dir" && "$forge" new forge-e2e-api --api --database none --skip-docker --skip-ci \
  --skip-lockfile --forge-path "$app_dir/.forge/crates/forge" >/dev/null)
[[ ! -e "$api_dir/templates" && ! -e "$api_dir/assets" ]] || fail "--api must not generate views"
(
  cd "$api_dir"
  cargo generate-lockfile --quiet
  cargo fmt --all -- --check
  cargo clippy --quiet --all-targets --locked -- -D warnings
  cargo test --quiet --locked
) || fail "--api application quality gates"

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

curl --silent --fail "http://127.0.0.1:3000/" | grep -q "<h1>$app_name</h1>" || fail "container index route"
docker compose logs app 2>&1 | grep -m1 -q ' | {' || fail "production logs must be JSON"

# The named volume must preserve the migrated schema across an application restart.
docker compose restart app >/dev/null
[[ "$(docker compose exec -T db psql -U postgres -d forge_e2e_app -Atc "SELECT to_regclass('public.forge_e2e_probe')")" == "forge_e2e_probe" ]]   || fail "database schema did not persist across app restart"

docker compose stop --timeout 20 app >/dev/null
app_container="$(docker compose ps --quiet --all app)"
exit_code="$(docker inspect --format '{{.State.ExitCode}}' "$app_container")"
[[ "$exit_code" == 0 ]] || { docker compose logs app >&2; fail "docker stop should exit 0, got $exit_code"; }

log "All end-to-end checks passed"
