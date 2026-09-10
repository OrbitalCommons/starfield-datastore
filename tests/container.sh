#!/usr/bin/env bash
set -euo pipefail
image=${1:?usage: container.sh IMAGE}
scratch=$(mktemp -d)
container="sfd-smoke-$(basename "$scratch")"
volume="$container-cache"
cleanup() {
  docker rm -f "$container" >/dev/null 2>&1 || true
  docker volume rm "$volume" >/dev/null 2>&1 || true
  rm -rf "$scratch"
}
trap cleanup EXIT

docker run --rm "$image" --version
test "$(docker run --rm --entrypoint id "$image" -u)" = 10001
docker run --rm --read-only -v "$volume:/var/lib/starfield-datastore/cache" "$image" list

# Health and graceful shutdown need no AWS connection or archive credentials.
chmod 755 "$scratch"
cat > "$scratch/config.toml" <<'TOML'
[[credentials]]
name = "future"
hosts = ["example.org"]
type = "future-auth"
opaque = "do-not-log-this-payload"
[services.smoke]
manifest = "/usr/share/starfield-datastore/ephemeris.toml"
bucket = "s3://unused-smoke-bucket"
region = "us-east-1"
bind = "127.0.0.1:18080"
credentials = ["future"]
TOML
docker run -d --name "$container" --network host --read-only \
  -v "$volume:/var/lib/starfield-datastore/cache" \
  -v "$scratch:/run/sfd-smoke:ro" \
  -e AWS_EC2_METADATA_DISABLED=true \
  "$image" --config /run/sfd-smoke/config.toml --service smoke serve
for attempt in $(seq 1 30); do
  if curl --fail --silent http://127.0.0.1:18080/healthz > /dev/null; then
    break
  fi
  sleep 1
done
curl --fail --silent http://127.0.0.1:18080/healthz > /dev/null
logs=$(docker logs "$container" 2>&1)
[[ "$logs" == *"future-auth"* && "$logs" == *"skipped"* ]]
[[ "$logs" != *"do-not-log-this-payload"* ]]
docker stop --time 10 "$container" > /dev/null
test "$(docker inspect -f '{{.State.ExitCode}}' "$container")" = 0
