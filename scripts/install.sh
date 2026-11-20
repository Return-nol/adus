#!/usr/bin/env bash
# One-command installer for Adus: creates .env, generates secrets, starts the stack.
# Safe to run again: values already in .env are never overwritten.
set -euo pipefail
umask 077 # .env holds secrets: files this script writes are private to the owner

die() { echo "ERROR: $*" >&2; exit 1; }

usage() {
  cat <<USAGE >&2
Usage: $0 [--build]

  --build   build the adus image from this checkout instead of pulling it
            (use it to run the latest source)
USAGE
  exit 1
}

build=
for arg in "$@"; do
  case "$arg" in
    --build) build=1 ;;
    *) usage ;;
  esac
done

cd "$(dirname "$0")/.."

# Print the value of KEY from .env (empty if missing).
get() { grep -m1 "^$1=" .env | cut -d= -f2- | tr -d '\r' || true; }

# Set KEY=VALUE in .env: replace the line for KEY, or append it if there is none.
set_var() {
  K="$1" V="$2" awk 'BEGIN { k = ENVIRON["K"] "="; v = ENVIRON["V"] }
    index($0, k) == 1 { print k v; found = 1; next } { print }
    END { if (!found) print k v }' .env > .env.tmp && mv .env.tmp .env
}

# 1. Docker
docker info >/dev/null 2>&1 \
  || die "Docker is not installed or not running. Install it from https://docs.docker.com/get-docker/ and start it."
compose_version=$(docker compose version --short 2>/dev/null) \
  || die "Docker Compose v2 is missing. See https://docs.docker.com/compose/install/"
compose_version=${compose_version#v}
compose_major=${compose_version%%.*}
compose_minor=${compose_version#*.}
compose_minor=${compose_minor%%.*}
if [ "$compose_major" -lt 2 ] || { [ "$compose_major" -eq 2 ] && [ "$compose_minor" -lt 20 ]; }; then
  die "Docker Compose 2.20 or newer is required (found $compose_version). See https://docs.docker.com/compose/install/"
fi
command -v curl >/dev/null 2>&1 || die "curl is required (used to wait for Adus to become healthy). Please install it."

# 2. .env
if [ ! -f .env ]; then
  cp .env.example .env
  echo "Created .env from .env.example"
fi
chmod 600 .env

# 3. Secrets (only when empty)
gen_secret() {
  local err
  err=$(mktemp)
  docker compose run --rm --no-deps -T adus gen-secret 2>"$err" | tr -d '\r' | tail -n 1 \
    || { cat "$err" >&2; rm -f "$err"; die "could not run 'adus gen-secret'"; }
  rm -f "$err"
}
if [ -z "$(get ADUS_SECRET_KEY)" ]; then
  secret=$(gen_secret)
  [ -n "$secret" ] || die "'adus gen-secret' printed nothing"
  set_var ADUS_SECRET_KEY "$secret"
  echo "Generated ADUS_SECRET_KEY (back up .env: losing it makes stored PATs and LLM keys unreadable)"
fi
if [ -z "$(get POSTGRES_PASSWORD)" ]; then
  password=$(gen_secret | tr -d '+/=')
  [ -n "$password" ] || die "'adus gen-secret' printed nothing"
  set_var POSTGRES_PASSWORD "$password"
  echo "Generated POSTGRES_PASSWORD"
fi

# 4. Public URL
port=$(get ADUS_PORT)
port=${port:-8080}
if [ -z "$(get ADUS_PUBLIC_URL)" ]; then
  default="http://localhost:$port"
  url=$default
  if [ -t 0 ]; then
    echo "Public URL: the address your browser and GitHub/Azure DevOps use to reach Adus."
    echo "Webhooks cannot reach localhost; use a public https URL (or a tunnel) for real reviews."
    read -r -p "ADUS_PUBLIC_URL [$default]: " answer || true
    url=${answer:-$default}
  fi
  set_var ADUS_PUBLIC_URL "$url"
  echo "ADUS_PUBLIC_URL=$url"
fi

# 5. Validate
if ! check=$(docker compose run --rm --no-deps -T adus check-config 2>&1); then
  echo "$check" >&2
  die "configuration errors above; fix .env and run this script again"
fi
echo "$check" | grep '^WARN' || true

# 6. Start
if [ -n "$build" ]; then
  docker compose build adus || die "could not build the adus image"
else
  # The adus image on its own first: if it cannot be pulled (offline, no release yet), build it here.
  if ! docker compose pull adus; then
    echo "Could not pull the adus image; building it from this checkout"
    docker compose build adus || die "could not build the adus image"
  fi
fi
# The other images (Postgres, Redis); adus was handled above.
docker compose pull --ignore-buildable || echo "Could not pull some images; continuing with local images"
docker compose up -d

# 7. Wait for /healthz
echo "Waiting for Adus to become healthy (up to 180 s)..."
for _ in $(seq 1 60); do
  if curl -fsS "http://localhost:$port/healthz" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 3
done
if [ -z "${ready:-}" ]; then
  docker compose logs --tail 50 adus >&2 || true
  die "Adus did not become healthy; see the logs above"
fi

# 8. Next steps
cat <<DONE

Adus is running.

  Dashboard:  $(get ADUS_PUBLIC_URL)  (local: http://localhost:$port)

Next steps:
  1. Open $(get ADUS_PUBLIC_URL)/setup and create the first admin account.
  2. Add an AI provider (AI Providers page) with at least one API key.
  3. Choose a default model in Settings.
  4. Add a connection (GitHub or Azure DevOps PAT) and pick repositories.

Check that reviews can run:  docker compose exec adus adus doctor
DONE
