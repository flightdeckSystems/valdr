# Docker

`valdr` ships as a single `redis-server` binary inside a small Debian
runtime image. The container listens on port 6379 and stores persistence files
under `/data`.

## Pull

Published images live at GitHub Container Registry:

```bash
docker pull ghcr.io/ianm199/valdr:alpha &&
docker run --rm -p 6379:6379 -v valdr-data:/data ghcr.io/ianm199/valdr:alpha
```

One-copy try/smoke flow using only Docker:

```bash
docker network create valdr-try >/dev/null 2>&1 || true
docker rm -f valdr-try >/dev/null 2>&1 || true
docker pull ghcr.io/ianm199/valdr:alpha
docker run -d --name valdr-try --network valdr-try -v valdr-data:/data ghcr.io/ianm199/valdr:alpha
docker run --rm --network valdr-try redis:7-alpine redis-cli -h valdr-try PING
docker run --rm --network valdr-try redis:7-alpine redis-cli -h valdr-try SET hello world
docker run --rm --network valdr-try redis:7-alpine redis-cli -h valdr-try GET hello
```

Stop it when done:

```bash
docker rm -f valdr-try
docker network rm valdr-try
```

Useful tags:

- `alpha` — latest alpha image from `main`.
- `main` — latest image from the default branch.
- `sha-<git-sha>` — immutable image for a specific commit.

Published images target `linux/amd64` and `linux/arm64`.

If the package is not visible yet, make the GHCR package public from the
repository package settings after the first workflow publish.

## Build locally

```bash
docker build -t valdr:local .
docker run --rm -p 6379:6379 -v valdr-data:/data valdr:local
```

Or with Compose:

```bash
docker compose up --build
```

## Smoke test

The Docker smoke builds the image, starts a container with a named volume, uses
`redis-py` to exercise `PING`, `SET`, `GET`, `HSET`, pipelining, and `SAVE`,
then restarts the container and verifies the data was reloaded from RDB:

```bash
bash harness/docker/smoke.sh
```

Set `IMAGE=...` to test a different image name:

```bash
docker pull ghcr.io/ianm199/valdr:alpha
SKIP_BUILD=1 IMAGE=ghcr.io/ianm199/valdr:alpha bash harness/docker/smoke.sh
```

## Benchmark with Docker

`harness/docker/bench.sh` starts the published image in an isolated Docker
network and runs `redis-benchmark` from `redis:7-alpine` against it. It does
not require a local Redis/Valkey install:

```bash
IMAGE=ghcr.io/ianm199/valdr:alpha \
REQUESTS=100000 \
CLIENTS=50 \
PIPELINE=16 \
TESTS=ping_inline,ping_mbulk,set,get,incr,lrange_100,lrange_300 \
bash harness/docker/bench.sh
```

Useful variants:

```bash
# Deep-pipeline smoke, similar to the public pipeline regression check.
PIPELINE=100 REQUESTS=200000 TESTS=get,set,incr,ping_mbulk bash harness/docker/bench.sh

# CSV output for spreadsheets or quick comparisons.
CSV=1 OUTPUT=harness/bench/results/docker-alpha.csv bash harness/docker/bench.sh

# Benchmark a locally built image without pulling.
docker build -t valdr:local .
PULL=0 IMAGE=valdr:local bash harness/docker/bench.sh
```

## Runtime config

The image runs:

```bash
redis-server /etc/valdr/redis.conf
```

The bundled config is:

```conf
bind 0.0.0.0
port 6379
dir /data
dbfilename dump.rdb
appendonly no
```

For persistence, mount `/data` as either a named volume or a writable host
directory.

## Publish

Publish from a machine logged into GHCR with package-write permission:

```bash
SHA="$(git rev-parse --short HEAD)"
IMAGE="ghcr.io/ianm199/valdr"

docker build \
  -t "$IMAGE:alpha" \
  -t "$IMAGE:main" \
  -t "$IMAGE:sha-$SHA" \
  .

echo "$GITHUB_TOKEN" | docker login ghcr.io -u ianm199 --password-stdin
docker push "$IMAGE:alpha"
docker push "$IMAGE:main"
docker push "$IMAGE:sha-$SHA"
```

## Current limits

The image has the same limits as the binary:

- single-node only, no cluster mode;
- no loadable C-ABI modules;
- alpha status until sustained-load testing and broader workload evidence are
  published.
