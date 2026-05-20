#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

START_SERVICE=0
for arg in "$@"; do
  case "$arg" in
    --start)
      START_SERVICE=1
      ;;
    -h|--help)
      printf 'usage: ./install.sh [--start]\n'
      exit 0
      ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

ROOT="$(pwd)"
RUNTIME_DIR="${ADDRESS_RESOLVER_RUNTIME_DIR:-$ROOT/out/runtime}"
MAP_DIR="${VALIDATORS_CLOCK_TON_MAP_DIR:-$ROOT/out/ton_map}"
CONFIG_PATH="${ADDRESS_RESOLVER_CONFIG:-$ROOT/address_resolver.json}"
SERVICE_NAME="${ADDRESS_RESOLVER_SERVICE_NAME:-address-resolver-ton.service}"
BIN="$ROOT/target/release/address_resolver"
HELPER="$ROOT/tools/ton-dht-resolver/ton-dht-resolver"

export PATH="$HOME/.cargo/bin:/usr/local/go/bin:$PATH"

run_privileged() {
  if [[ "$(id -u)" -eq 0 ]]; then
    "$@"
  elif command -v sudo >/dev/null; then
    sudo "$@"
  else
    echo "missing sudo; cannot run: $*" >&2
    exit 1
  fi
}

install_apt_packages() {
  if ! command -v apt-get >/dev/null; then
    echo "automatic dependency install currently supports Debian/Ubuntu with apt-get" >&2
    return 1
  fi

  run_privileged apt-get update
  run_privileged apt-get install -y --no-install-recommends "$@"
}

ensure_rust() {
  if command -v rustup >/dev/null; then
    echo "updating Rust toolchain"
    rustup update stable
    rustup default stable
  elif command -v cargo >/dev/null; then
    echo "using existing Rust toolchain: $(cargo --version)"
  else
    echo "installing Rust toolchain"
    install_apt_packages ca-certificates curl build-essential pkg-config
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain stable
    export PATH="$HOME/.cargo/bin:$PATH"
  fi

  command -v cargo >/dev/null || {
    echo "cargo is still missing after Rust setup" >&2
    exit 1
  }
}

ensure_go() {
  if command -v go >/dev/null; then
    echo "using existing Go toolchain: $(go version)"
    return
  fi

  echo "installing Go toolchain"
  install_apt_packages ca-certificates build-essential pkg-config golang-go

  command -v go >/dev/null || {
    echo "go is still missing after Go setup" >&2
    exit 1
  }
}

if [[ "${ADDRESS_RESOLVER_SKIP_DEPS:-0}" == "1" ]]; then
  echo "skipping dependency setup"
else
  ensure_rust
  ensure_go
fi

for cmd in cargo go; do
  command -v "$cmd" >/dev/null || {
    echo "missing command: $cmd" >&2
    exit 1
  }
done

mkdir -p "$RUNTIME_DIR" "$MAP_DIR"

echo "building Rust collector"
cargo build --release

echo "building TON DHT helper"
(
  cd tools/ton-dht-resolver
  GOCACHE="${GOCACHE:-/tmp/address-resolver-go-build}" \
  GOPATH="${GOPATH:-/tmp/address-resolver-go}" \
  go build -o ton-dht-resolver .
)

if [[ -f "$CONFIG_PATH" ]]; then
  echo "keeping existing config: $CONFIG_PATH"
else
  echo "creating config: $CONFIG_PATH"
  cat > "$CONFIG_PATH" <<EOF_CONFIG
{
  "base_url": "https://validatorsclock.xyz",
  "chain": "ton",
  "interval_secs": 60,
  "full_geo_refresh_secs": 3600,
  "state": "$RUNTIME_DIR/ton_nodes_state.json",
  "output": "$MAP_DIR/ton_full.json",
  "map_output": "$MAP_DIR/ton_nodes.json",
  "compact": true,
  "resolver": {
    "kind": "ton-dht",
    "command": "$HELPER",
    "ton_config_url": "https://ton-blockchain.github.io/global.config.json",
    "ton_workers": 16,
    "ton_batch_timeout_secs": 600,
    "ton_lookup_timeout_secs": 30
  },
  "geo": {
    "endpoint": "http://ip-api.com/batch?fields=status,message,country,countryCode,regionName,city,lat,lon,isp,org,as,query",
    "batch_size": 100,
    "cache": "$RUNTIME_DIR/ton_geo_cache.json"
  }
}
EOF_CONFIG
fi

if command -v systemctl >/dev/null; then
  USER_UNIT_DIR="$HOME/.config/systemd/user"
  UNIT_PATH="$USER_UNIT_DIR/$SERVICE_NAME"
  if mkdir -p "$USER_UNIT_DIR" 2>/dev/null; then
    if [[ -w "$USER_UNIT_DIR" ]]; then
      echo "writing systemd user unit: $UNIT_PATH"
      if cat > "$UNIT_PATH" <<EOF_SERVICE
[Unit]
Description=TON validator address resolver
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=$ROOT
ExecStart=$BIN run --config $CONFIG_PATH
Restart=always
RestartSec=15

[Install]
WantedBy=default.target
EOF_SERVICE
      then
        systemctl --user daemon-reload || true

        if [[ "$START_SERVICE" -eq 1 ]]; then
          systemctl --user enable --now "$SERVICE_NAME"
        else
          echo "service is installed but not started"
          echo "start it with: systemctl --user enable --now $SERVICE_NAME"
        fi
      else
        echo "systemd user unit not installed: cannot write $UNIT_PATH" >&2
      fi
    else
      echo "systemd user unit not installed: cannot write $USER_UNIT_DIR" >&2
    fi
  else
    echo "systemd user unit not installed: cannot create $USER_UNIT_DIR" >&2
  fi
fi

echo "installed"
echo "config: $CONFIG_PATH"
echo "run once: $BIN run --config $CONFIG_PATH --once"
