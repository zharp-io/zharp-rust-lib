#!/bin/sh
# Runs every end-to-end case in Docker and leaves what the apps sent in e2e/out/otlp.jsonl.
#   sh e2e/run.sh          (from the repo root; needs only Docker)
set -e
cd "$(dirname "$0")"
mkdir -p out && rm -f out/otlp.jsonl
# Crates unpacked one app at a time: builds sharing the registry cache can't unpack at once.
for app in receiver shop carts; do docker compose run --rm --no-deps --entrypoint "cargo fetch -q" $app; done
docker compose --profile cli run --rm --no-deps --entrypoint "cargo fetch -q" cli
docker compose up -d --wait receiver postgres mysql shop shop-stripped carts
curl_() { docker run --rm --network "$(basename "$PWD")_default" curlimages/curl:8.11.1 -s -o /dev/null -w "%{http_code} $*\n" "$@"; }
for host in shop shop-stripped; do
  curl_ -H 'traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01' -H 'authorization: Bearer secret' -H 'baggage: enduser.id=u_7,session.id=s_1' "http://$host:3000/orders/42"
  curl_ -X POST "http://$host:3000/signup"
  curl_ "http://$host:3000/carts"
  curl_ "http://$host:3000/panic"
done
curl_ -X POST http://carts:8080/carts
curl_ http://carts:8080/down
curl_ http://carts:8080/carts/3
docker compose --profile cli run --rm cli missing.csv || true
docker compose --profile cli run --rm cli panic || true
sleep 7 # the batch processors send every 5 s
docker compose logs shop carts --no-log-prefix | tail -20
echo "$(wc -l < out/otlp.jsonl) exports in e2e/out/otlp.jsonl"
