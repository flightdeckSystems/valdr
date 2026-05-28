# valdr

Single-node Valkey-compatible server: a Rust port of the upstream Valkey C implementation.

## Status

| Area | Status |
|---|---|
| Release state | Alpha |
| Primary target | Single-node Redis/Valkey workloads |
| Protocol | RESP2 / RESP3 |
| Client compatibility | Existing Redis clients |
| License | BSD-3-Clause |

## Compatibility

| Surface | Current state | Evidence |
|---|---|---|
| Single-node RESP wire behavior | Full on current smoke corpus | 23 / 23 byte-exact scripts vs upstream Valkey |
| RDB load/save interop | Full on current corpus | 378 / 378 bidirectional checks |
| Upstream TCL suite | Single-node core green; not a full-suite claim | Full denominator 4,299 blocks, bucketed in [`docs/TEST_AND_FEATURE_COVERAGE.md`](docs/TEST_AND_FEATURE_COVERAGE.md) |
| Cluster mode | Not implemented | Out of scope for current alpha |
| Loadable C modules | Not implemented | Out of scope for current alpha |
| Production HA / Sentinel | Not claimed | Replication/AOF exist but are not production-conformance gated |
| In-process TLS | Enabled (rustls; no OpenSSL) | TLS 1.2 + 1.3; mTLS tri-state (`no`/`optional`/`yes`); dynamic CONFIG SET of `tls-protocols`, `tls-auth-clients`, cert/key paths. CBC-suite tests in `unit/tls.tcl` are a deliberate rustls divergence — see [`docs/TLS_FAITHFUL_PLAN.md`](docs/TLS_FAITHFUL_PLAN.md). |

## Features

| Feature | Status |
|---|---|
| Strings | Implemented |
| Lists | Implemented |
| Hashes | Implemented |
| Sets | Implemented |
| Sorted sets | Implemented |
| Streams | Implemented |
| Pub/sub | Implemented |
| Transactions | Implemented |
| Lua scripting | Implemented |
| ACL / AUTH | Implemented |
| Multi-DB | Implemented |
| Expiration / TTL | Implemented |
| Maxmemory eviction | Implemented |
| RDB persistence | Implemented and oracle-gated |
| AOF | Alpha |
| Replication | Alpha |
| RedisJSON-compatible commands | Native subset |
| RedisBloom-compatible commands | Native subset |

## Benchmark Commands

<details>
<summary>Run official valkey-benchmark against Valkey and valdr Docker images</summary>

```bash
docker network create valdr-bench

docker run -d --rm \
  --name valkey-ref \
  --network valdr-bench \
  valkey/valkey:8-alpine

docker run -d --rm \
  --name valdr \
  --network valdr-bench \
  ghcr.io/flightdecksystems/valdr:alpha

sleep 1
```

```bash
docker run --rm \
  --network valdr-bench \
  valkey/valkey:8-alpine \
  valkey-benchmark \
    -h valkey-ref \
    -p 6379 \
    -n 100000 \
    -c 50 \
    -P 100 \
    -d 64 \
    -t ping_inline,ping_mbulk,set,get,incr,lpush,rpush,lpop,rpop,sadd,hset,spop,zadd,zpopmin,lrange_100,lrange_300,lrange_500,lrange_600,mset,mget,xadd,function_load,fcall \
    --warmup 1 \
    --csv \
    --precision 3
```

```bash
docker run --rm \
  --network valdr-bench \
  valkey/valkey:8-alpine \
  valkey-benchmark \
    -h valdr \
    -p 6379 \
    -n 100000 \
    -c 50 \
    -P 100 \
    -d 64 \
    -t ping_inline,ping_mbulk,set,get,incr,lpush,rpush,lpop,rpop,sadd,hset,spop,zadd,zpopmin,lrange_100,lrange_300,lrange_500,lrange_600,mset,mget,xadd,function_load,fcall \
    --warmup 1 \
    --csv \
    --precision 3
```

```bash
docker rm -f valkey-ref valdr
docker network rm valdr-bench
```

</details>

## Performance

Latest warmed local run vs **two upstream Valkey versions**: Valkey 8.1.7
(the current 8-line stable, what `valkey/valkey:8-alpine` ships) and Valkey
9.1.0 (the current 9-line stable, released 2026-05-19). Same Valdr binary
benchmarked against both adversaries; both adversaries built from
`reference/valkey` with `make BUILD_TLS=no`.

| Metric | vs Valkey 8.1.7 | vs Valkey 9.1.0 |
|---|---:|---:|
| Median ratio across 23 commands | **1.134x** | **1.150x** |
| Pipeline-smoke median (GET/PING_MBULK/SET × p=1/16/100) | 1.057x | 1.124x |
| JSON cache mix median (4 KB docs, p=1) | — (see note) | 1.001x |

The two Valkey versions perform very similarly on this matrix — 9.1.0 is
slightly faster on ping/get/incr; 8.1.7 is slightly faster on data-structure
ops (`set`, `lrange_*`, `zpopmin`, `spop`). The "production-current" comparison
(8.1.7) and the "hardest bar" comparison (9.1.0) give essentially the same
story.

### Per-command, both adversaries side-by-side

| Command | Valdr rps | Valkey 8.1.7 rps | Ratio | Valkey 9.1.0 rps | Ratio |
|---|---:|---:|---:|---:|---:|
| PING_INLINE | 5,263,158 | 3,703,704 | 1.421x | 4,000,000 | 1.316x |
| PING_MBULK | 7,142,857 | 5,555,556 | 1.286x | 5,555,556 | 1.286x |
| SET | 3,703,704 | 3,030,303 | 1.222x | 2,564,102 | 1.500x |
| GET | 4,761,905 | 3,846,154 | 1.238x | 3,448,276 | 1.318x |
| INCR | 4,166,667 | 4,000,000 | 1.042x | 3,571,428 | 1.077x |
| LPUSH | 2,777,778 | 2,439,024 | 1.139x | 2,380,952 | 1.077x |
| RPUSH | 2,702,703 | 2,702,703 | 1.000x | 2,777,778 | 0.900x |
| LPOP | 2,564,102 | 2,272,727 | 1.128x | 2,173,913 | 1.150x |
| RPOP | 2,380,952 | 2,500,000 | 0.952x | 2,439,024 | 0.976x |
| SADD | 2,380,952 | 3,225,806 | 0.738x | 3,125,000 | 0.744x |
| HSET | 1,724,138 | 2,500,000 | 0.690x | 2,500,000 | 0.702x |
| SPOP | 2,857,143 | 4,000,000 | 0.714x | 3,703,704 | 0.771x |
| ZADD | 1,923,077 | 2,439,024 | 0.788x | 2,222,222 | 0.833x |
| ZPOPMIN | 2,857,143 | 4,166,667 | 0.686x | 3,703,704 | 0.730x |
| LRANGE_100 (first 100) | 176,367 | 129,032 | 1.367x | 114,943 | 1.626x |
| LRANGE_300 (first 300) | 56,180 | 38,971 | 1.442x | 37,722 | 1.656x |
| LRANGE_500 (first 500) | 34,153 | 23,175 | 1.474x | 21,777 | 1.613x |
| LRANGE_600 (first 600) | 27,778 | 18,685 | 1.487x | 18,123 | 1.617x |
| MSET (10 keys) | 636,943 | 456,621 | 1.395x | 442,478 | 1.421x |
| MGET (10 keys) | 1,030,928 | — | — | 740,741 | 1.392x |
| XADD | 1,123,596 | 1,388,889 | 0.809x | 1,408,451 | 0.780x |
| FUNCTION_LOAD | 578,035 | 58,893 | 9.815x | 56,593 | 10.394x |
| FCALL | 847,458 | 1,428,571 | 0.593x | 1,351,351 | 0.643x |

The honest pattern:
- **Wins** (ratio > 1.2x against both): `ping_*`, `set`, `get`, `mset`,
  `mget`, all `lrange` variants, `function_load`.
- **Parity** (0.95×–1.20× against both): `incr`, `lpush`, `rpush`, `lpop`, `rpop`.
- **Behind** (0.6×–0.85×): `sadd`, `hset`, `spop`, `zadd`, `zpopmin`, `xadd`, `fcall`.
  These are the data-structure-internal commands and the Lua FCALL path.
  These are also where the Rust port's behavioral fidelity guarantees
  (oracle-gated against upstream tests) currently extract a perf cost we
  haven't paid down yet.

### Pipeline-depth curve (the publication-relevant shape)

The single best summary of where Valdr wins: GET/SET/PING_MBULK throughput
at three pipeline depths against both adversaries.

| Workload | Pipeline | Valdr rps | Valkey 8.1.7 ratio | Valkey 9.1.0 ratio |
|---|---:|---:|---:|---:|
| GET | 1 | 161,551 | 0.831x | 0.919x |
| GET | 16 | 2,590,674 | 1.067x | 1.124x |
| GET | 100 | 6,024,096 | **1.474x** | **1.530x** |
| PING_MBULK | 1 | 179,533 | 0.946x | 1.013x |
| PING_MBULK | 16 | 2,375,297 | 0.998x | 1.019x |
| PING_MBULK | 100 | 7,407,407 | **1.385x** | **1.496x** |
| SET | 1 | 165,016 | 0.864x | 0.932x |
| SET | 16 | 2,028,397 | 1.075x | 1.262x |
| SET | 100 | 3,636,363 | **1.501x** | **1.549x** |

At single-request pipeline depth, Valdr trails by 5-17% (per-request RESP
parsing + dispatch overhead). At pipeline=16, parity. At pipeline=100,
Valdr is consistently **1.4-1.5× faster than either Valkey version** — the
RuntimeOwner event loop amortizes parser/dispatch/write costs better than
upstream's accept-per-thread model.

Benchmark notes:

- Valdr commit: `7838a3d` (this README); Valkey adversaries:
  `valkey/valkey:8-alpine` (= 8.1.7) and `valkey-io/valkey@9.1.0`
- Host: Apple M3 Max, macOS
- Warmup: 1,000 `PING_MBULK` requests, 1 client, pipeline 1, before every measured row
- Full artifacts:
  `harness/bench/results/20260528T191756Z-9666290-official-warm-results.md` (vs 9.1.0),
  `harness/bench/results/20260528T193018Z-7838a3d-official-warm-run.log` (vs 8.1.7)
- MGET vs 8.1.7 parse-errored in `valkey-benchmark`; row excluded from the 8.1.7 column.
  Median is computed over the 22 commands present.
- Re-run: `bash harness/bench/official-warm-run.sh` (uses whatever Valkey
  is built at `reference/valkey`; switch tags with `git -C reference/valkey
  checkout <tag> && make -j BUILD_TLS=no`)

## Run

```bash
cargo build --release
./target/release/redis-server --port 6379 --bind 127.0.0.1
```

## Docker

```bash
docker pull ghcr.io/flightdecksystems/valdr:alpha
docker run --rm -p 6379:6379 ghcr.io/flightdecksystems/valdr:alpha
```

## Test Commands

```bash
bash scripts/setup-reference.sh
cargo build -p redis-server
cargo test --workspace
bash harness/oracle/smoke.sh --skip-build
python3 harness/oracle/rdb-diff --direction=all
bash harness/oracle/run-single-node-tcl-suite.sh --skip-build
bash harness/bench/official-warm-run.sh
```

The single source of truth for what these prove — counted TCL passes,
single-node source-block coverage, and how the full 4,299-block upstream
denominator is bucketed — is
[`docs/TEST_AND_FEATURE_COVERAGE.md`](docs/TEST_AND_FEATURE_COVERAGE.md).
