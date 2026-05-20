# address_resolver

Standalone collector for validator network addresses used by `validators_clock` maps.

The collector reads the active validator set from
`https://validatorsclock.xyz/api/chains/<chain>/clock`, normalizes
`validator_public_key -> adnl_addr`, resolves ADNL addresses through a resolver
backend, and writes a JSON file. Network resolution is deliberately kept outside
the website request path.

## Fetch active validators

```bash
cargo run -- collect --chain ton --limit 5 --output out/ton.json
cargo run -- collect --chain everscale --limit 5 --output out/everscale.json
```

With the default `none` resolver, records are emitted with
`resolution.status = "not_attempted"`. This is useful for validating the input
API and output schema.

## TON DHT resolver

The `ton-dht` resolver starts one TON ADNL/DHT client through
`tools/ton-dht-resolver`, then resolves all active validator `adnl_addr` values
in one batch. The TON global config is used only as bootstrap; the helper keeps
one DHT client alive for the batch so lookups can reuse the DHT view learned
while walking the network.

Build the helper:

```bash
cd tools/ton-dht-resolver
GOCACHE=/tmp/address-resolver-go-build GOPATH=/tmp/address-resolver-go go build -o ton-dht-resolver .
```

Resolve the full active TON validator set:

```bash
cargo run -- collect \
  --chain ton \
  --resolver ton-dht \
  --ton-workers 16 \
  --ton-batch-timeout-secs 600 \
  --ton-lookup-timeout-secs 30 \
  --output out/ton-full.json
```

Build the map JSON with geo data:

```bash
cargo run -- collect \
  --chain ton \
  --resolver ton-dht \
  --ton-workers 16 \
  --ton-batch-timeout-secs 600 \
  --ton-lookup-timeout-secs 30 \
  --output out/ton-full.json \
  --map-output out/ton-map.json \
  --geo-cache out/geo-cache.json
```

`--map-output` writes the legacy array format expected by the current
`validators_clock` map code. Each resolved validator is one row with exactly
`peer`, `ip`, `city`, `country`, `isp`, `lat`, and `lon`. For TON, `peer` is the
validator public key from the current validator set. If DHT returns multiple
addresses for one validator, the exporter picks one canonical IP for this legacy
file and keeps the full address list in `out/ton-full.json`.

## Install

Production install is meant to be one command after clone:

```bash
git clone <repo-url> address_resolver
cd address_resolver
./install.sh
```

`install.sh` builds the Rust collector, builds the Go TON DHT helper, creates
`address_resolver.json` if it does not exist, and installs a user systemd unit
named `address-resolver-ton.service` when systemd is available.

It also installs missing build dependencies. Rust is installed or updated
through `rustup`; Go is installed through `apt-get` on Debian/Ubuntu when `go`
is missing.

Default generated paths:

- runtime state and geo cache: `./out/runtime`
- TON map output: `./out/ton_map/ton_nodes.json`
- full resolver output: `./out/ton_map/ton_full.json`

For a production `validators_clock` user, point the map output directory at the
directory read by the website:

```bash
VALIDATORS_CLOCK_TON_MAP_DIR=/home/admin/.validators_clock/ton_map ./install.sh
```

To start the service immediately:

```bash
./install.sh --start
```

Or start it later:

```bash
systemctl --user enable --now address-resolver-ton.service
```

## Config-driven run

Production runs should use `address_resolver.json`, not long CLI commands. The
tracked `address_resolver.example.json` documents the schema; `install.sh`
generates a local `address_resolver.json` with absolute paths.

Run one iteration:

```bash
target/release/address_resolver run --once
```

Run forever:

```bash
target/release/address_resolver run
```

Use a non-default config:

```bash
target/release/address_resolver run --config /path/to/address_resolver.json
```

The production shape is a long-running Rust collector plus the current Go
`tonutils-go` DHT helper:

- Rust binary: scheduling, validatorsclock.xyz API, output files, state, geo
  cache, and full refresh policy.
- Go helper: low-level TON ADNL/DHT lookup backend, because `tonutils-go`
  already has a working network stack.

On every iteration the collector rebuilds the full TON map from the current
validator round. New IP addresses are sent to `ip-api.com`; known IP addresses
are served from the configured geo cache. Every `full_geo_refresh_secs` seconds
the collector refreshes all currently resolved IP addresses and stores the next
refresh checkpoint in the configured state file.

The state file is operational metadata only. If it is deleted, the next run will
do a full geo refresh and recreate it. The map file is written atomically via a
temporary file and rename, so `validators_clock` should never see a half-written
JSON file.

Current observed result on a full TON map run:

- 400 active validators with `adnl_addr`
- 397 resolved validators
- 397 map rows
- 338 unique IPs with geo data

## External command resolver

The `command` resolver is a bridge for early DHT experiments. It calls an
external program once per validator and extracts IP addresses from either JSON
or plain text stdout.

Available template variables:

- `{chain_id}`
- `{validator_public_key}`
- `{adnl_addr}`

Example shape:

```bash
cargo run -- collect \
  --chain ton \
  --limit 10 \
  --resolver command \
  --command ./resolve-adnl \
  --arg '{adnl_addr}' \
  --output out/ton-resolved.json
```

Supported stdout formats include:

```json
[{"ip":"1.2.3.4","port":30303}]
```

or plain text containing socket addresses:

```text
Found 1.2.3.4:30303 / key
```

## TON DHT helper

`tools/ton-dht-resolver` is a Go helper using `tonutils-go`. It can resolve one
ADNL address or read a batch from stdin. It accepts any TON-compatible global
config URL, but the working path is currently TON mainnet.

```bash
cd tools/ton-dht-resolver
go run . ff0feaa19326615e62defde8919a2ff4087b60ba8aede8139f201b75665a0093
```

Batch mode directly:

```bash
printf '{"adnl_addrs":["ff0feaa19326615e62defde8919a2ff4087b60ba8aede8139f201b75665a0093"]}' \
  | ./ton-dht-resolver --batch --workers 8 --timeout 60s --per-lookup-timeout 20s
```

Observed status:

- Everscale: using `https://raw.githubusercontent.com/everx-labs/main.ton.dev/master/configs/ton-global.config.json`
  with the same helper reached DHT but returned `value is not found` for the
  first checked active validator ADNLs. Next step is to test the same keys with
  native `ever-node`/`ever-adnl` tooling before deciding whether the issue is
  DHT coverage, global config freshness, or a chain-specific key format.
