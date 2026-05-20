mod client;
mod geo;
mod map_export;
mod model;
mod resolver;

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use client::ValidatorsClockClient;
use geo::{GeoCache, GeoClient};
use map_export::{build_cached_map_nodes, collect_resolved_ips};
use model::{CollectionOutput, OutputChain, ResolverMetadata, ValidatorRecord};
use resolver::{ResolveRequest, ResolverKind, build_resolver};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

const DEFAULT_GEO_ENDPOINT: &str = "http://ip-api.com/batch?fields=status,message,country,countryCode,regionName,city,lat,lon,isp,org,as,query";
const DEFAULT_MAP_STALE_AFTER_SECS: u64 = 3600;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[arg(
        long,
        env = "VALIDATORS_CLOCK_BASE_URL",
        default_value = "https://validatorsclock.xyz"
    )]
    base_url: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Collect(CollectArgs),
    CollectLoop(CollectLoopArgs),
    Run(RunArgs),
}

#[derive(Clone, Debug, Args)]
struct CollectArgs {
    #[arg(long, default_value = "ton")]
    chain: String,

    #[arg(long)]
    limit: Option<usize>,

    #[arg(long, value_enum, default_value_t = ResolverKind::None)]
    resolver: ResolverKind,

    #[arg(long)]
    command: Option<PathBuf>,

    #[arg(long = "arg")]
    command_args: Vec<String>,

    #[arg(long, default_value_t = 10)]
    command_timeout_secs: u64,

    #[arg(
        long,
        default_value = "https://ton-blockchain.github.io/global.config.json"
    )]
    ton_config_url: String,

    #[arg(long, default_value_t = 8)]
    ton_workers: usize,

    #[arg(long, default_value_t = 300)]
    ton_batch_timeout_secs: u64,

    #[arg(long, default_value_t = 20)]
    ton_lookup_timeout_secs: u64,

    #[arg(short, long)]
    output: Option<PathBuf>,

    #[arg(long)]
    geo: bool,

    #[arg(long, default_value = DEFAULT_GEO_ENDPOINT)]
    geo_endpoint: String,

    #[arg(long, default_value_t = 100)]
    geo_batch_size: usize,

    #[arg(long)]
    geo_cache: Option<PathBuf>,

    #[arg(long)]
    map_output: Option<PathBuf>,

    #[arg(long)]
    map_cache: Option<PathBuf>,

    #[arg(long, default_value_t = DEFAULT_MAP_STALE_AFTER_SECS)]
    map_stale_after_secs: u64,

    #[arg(long)]
    compact: bool,
}

#[derive(Debug, Args)]
struct CollectLoopArgs {
    #[command(flatten)]
    collect: CollectArgs,

    #[arg(long, default_value_t = 60)]
    interval_secs: u64,

    #[arg(long, default_value_t = 3600)]
    full_geo_refresh_secs: u64,

    #[arg(long)]
    state: Option<PathBuf>,

    #[arg(long)]
    once: bool,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[arg(short, long, default_value = "address_resolver.json")]
    config: PathBuf,

    #[arg(long)]
    once: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddressResolverConfig {
    #[serde(default = "default_base_url")]
    base_url: String,
    #[serde(default = "default_chain")]
    chain: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    resolver: ResolverConfig,
    #[serde(default)]
    output: Option<PathBuf>,
    #[serde(default)]
    map_output: Option<PathBuf>,
    #[serde(default)]
    map_cache: Option<PathBuf>,
    #[serde(default = "default_map_stale_after_secs")]
    map_stale_after_secs: u64,
    #[serde(default)]
    geo: GeoConfig,
    #[serde(default)]
    compact: bool,
    #[serde(default = "default_interval_secs")]
    interval_secs: u64,
    #[serde(default = "default_full_geo_refresh_secs")]
    full_geo_refresh_secs: u64,
    #[serde(default)]
    state: Option<PathBuf>,
    #[serde(default)]
    once: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolverConfig {
    #[serde(default)]
    kind: ResolverKind,
    #[serde(default)]
    command: Option<PathBuf>,
    #[serde(default, alias = "args")]
    command_args: Vec<String>,
    #[serde(default = "default_command_timeout_secs")]
    command_timeout_secs: u64,
    #[serde(default = "default_ton_config_url")]
    ton_config_url: String,
    #[serde(default = "default_ton_workers")]
    ton_workers: usize,
    #[serde(default = "default_ton_batch_timeout_secs")]
    ton_batch_timeout_secs: u64,
    #[serde(default = "default_ton_lookup_timeout_secs")]
    ton_lookup_timeout_secs: u64,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            kind: ResolverKind::None,
            command: None,
            command_args: Vec::new(),
            command_timeout_secs: default_command_timeout_secs(),
            ton_config_url: default_ton_config_url(),
            ton_workers: default_ton_workers(),
            ton_batch_timeout_secs: default_ton_batch_timeout_secs(),
            ton_lookup_timeout_secs: default_ton_lookup_timeout_secs(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GeoConfig {
    #[serde(default = "default_geo_endpoint")]
    endpoint: String,
    #[serde(default = "default_geo_batch_size")]
    batch_size: usize,
    #[serde(default)]
    cache: Option<PathBuf>,
}

impl AddressResolverConfig {
    fn to_collect_loop_args(&self, config_path: &Path) -> CollectLoopArgs {
        let base_dir = config_base_dir(config_path);

        CollectLoopArgs {
            collect: CollectArgs {
                chain: self.chain.clone(),
                limit: self.limit,
                resolver: self.resolver.kind,
                command: self
                    .resolver
                    .command
                    .as_ref()
                    .map(|path| resolve_config_path(&base_dir, path)),
                command_args: self.resolver.command_args.clone(),
                command_timeout_secs: self.resolver.command_timeout_secs,
                ton_config_url: self.resolver.ton_config_url.clone(),
                ton_workers: self.resolver.ton_workers,
                ton_batch_timeout_secs: self.resolver.ton_batch_timeout_secs,
                ton_lookup_timeout_secs: self.resolver.ton_lookup_timeout_secs,
                output: self
                    .output
                    .as_ref()
                    .map(|path| resolve_config_path(&base_dir, path)),
                geo: false,
                geo_endpoint: self.geo.endpoint.clone(),
                geo_batch_size: self.geo.batch_size,
                geo_cache: self
                    .geo
                    .cache
                    .as_ref()
                    .map(|path| resolve_config_path(&base_dir, path)),
                map_output: self
                    .map_output
                    .as_ref()
                    .map(|path| resolve_config_path(&base_dir, path)),
                map_cache: self
                    .map_cache
                    .as_ref()
                    .map(|path| resolve_config_path(&base_dir, path))
                    .or_else(|| default_map_cache_path(&base_dir, self.state.as_ref())),
                map_stale_after_secs: self.map_stale_after_secs,
                compact: self.compact,
            },
            interval_secs: self.interval_secs,
            full_geo_refresh_secs: self.full_geo_refresh_secs,
            state: self
                .state
                .as_ref()
                .map(|path| resolve_config_path(&base_dir, path)),
            once: self.once,
        }
    }
}

fn default_base_url() -> String {
    "https://validatorsclock.xyz".to_owned()
}

fn default_chain() -> String {
    "ton".to_owned()
}

fn default_command_timeout_secs() -> u64 {
    10
}

fn default_ton_config_url() -> String {
    "https://ton-blockchain.github.io/global.config.json".to_owned()
}

fn default_ton_workers() -> usize {
    8
}

fn default_ton_batch_timeout_secs() -> u64 {
    300
}

fn default_ton_lookup_timeout_secs() -> u64 {
    20
}

fn default_geo_endpoint() -> String {
    DEFAULT_GEO_ENDPOINT.to_owned()
}

fn default_geo_batch_size() -> usize {
    100
}

fn default_interval_secs() -> u64 {
    60
}

fn default_full_geo_refresh_secs() -> u64 {
    3600
}

fn default_map_stale_after_secs() -> u64 {
    DEFAULT_MAP_STALE_AFTER_SECS
}

#[derive(Clone, Copy, Debug, Default)]
struct CollectRunOptions {
    force_geo_refresh: bool,
    suppress_stdout: bool,
}

#[derive(Debug)]
struct CollectSummary {
    generated_at: u64,
    validators_total: usize,
    validators_with_adnl: usize,
    resolved_total: usize,
    map_nodes: Option<usize>,
    geo_lookups: usize,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct CollectorState {
    #[serde(default)]
    last_time_full_check: u64,
    #[serde(default)]
    last_success_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    #[serde(default)]
    last_generated_at: u64,
    #[serde(default)]
    last_validators_total: usize,
    #[serde(default)]
    last_validators_with_adnl: usize,
    #[serde(default)]
    last_resolved_total: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_map_nodes: Option<usize>,
    #[serde(default)]
    last_geo_lookups: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Collect(args) => collect(cli.base_url, args).await,
        Commands::CollectLoop(args) => collect_loop(cli.base_url, args).await,
        Commands::Run(args) => run_from_config(args).await,
    }
}

async fn collect(base_url: String, args: CollectArgs) -> Result<()> {
    run_collect(&base_url, &args, CollectRunOptions::default())
        .await
        .map(|_| ())
}

async fn collect_loop(base_url: String, args: CollectLoopArgs) -> Result<()> {
    let interval = Duration::from_secs(args.interval_secs.max(1));
    let full_geo_refresh_secs = args.full_geo_refresh_secs.max(1);
    let mut state = load_collector_state(args.state.as_deref())?;

    if args.collect.geo_cache.is_none() && (args.collect.geo || args.collect.map_output.is_some()) {
        eprintln!(
            "warning: collect-loop without --geo-cache will call the geo endpoint on every run"
        );
    }

    loop {
        let started_at = unix_now();
        let has_geo_output = args.collect.geo || args.collect.map_output.is_some();
        let force_geo_refresh = has_geo_output
            && started_at.saturating_sub(state.last_time_full_check) >= full_geo_refresh_secs;

        eprintln!(
            "collect start chain={} resolver={} full_geo_refresh={}",
            args.collect.chain,
            args.collect.resolver.as_str(),
            force_geo_refresh
        );

        let options = CollectRunOptions {
            force_geo_refresh,
            suppress_stdout: true,
        };

        match run_collect(&base_url, &args.collect, options).await {
            Ok(summary) => {
                let finished_at = unix_now();

                if force_geo_refresh {
                    state.last_time_full_check = finished_at;
                }

                state.last_success_at = finished_at;
                state.last_error_at = None;
                state.last_error = None;
                state.last_generated_at = summary.generated_at;
                state.last_validators_total = summary.validators_total;
                state.last_validators_with_adnl = summary.validators_with_adnl;
                state.last_resolved_total = summary.resolved_total;
                state.last_map_nodes = summary.map_nodes;
                state.last_geo_lookups = summary.geo_lookups;

                save_collector_state(args.state.as_deref(), &state)?;

                eprintln!(
                    "collect ok validators={} with_adnl={} resolved={} map_nodes={} geo_lookups={}",
                    summary.validators_total,
                    summary.validators_with_adnl,
                    summary.resolved_total,
                    summary
                        .map_nodes
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "-".to_owned()),
                    summary.geo_lookups
                );
            }
            Err(err) => {
                let finished_at = unix_now();
                state.last_error_at = Some(finished_at);
                state.last_error = Some(format!("{err:#}"));
                save_collector_state(args.state.as_deref(), &state)?;
                eprintln!("collect failed: {err:#}");

                if args.once {
                    return Err(err);
                }
            }
        }

        if args.once {
            break;
        }

        let elapsed = Duration::from_secs(unix_now().saturating_sub(started_at));
        if let Some(sleep_for) = interval.checked_sub(elapsed) {
            sleep(sleep_for).await;
        }
    }

    Ok(())
}

async fn run_from_config(args: RunArgs) -> Result<()> {
    let config = load_config_file(&args.config)?;
    let mut loop_args = config.to_collect_loop_args(&args.config);

    if args.once {
        loop_args.once = true;
    }

    collect_loop(config.base_url, loop_args).await
}

async fn run_collect(
    base_url: &str,
    args: &CollectArgs,
    options: CollectRunOptions,
) -> Result<CollectSummary> {
    let client = ValidatorsClockClient::new(base_url);
    let clock = client.fetch_clock(&args.chain).await?;

    let resolver = build_resolver(
        args.resolver,
        args.command.clone(),
        args.command_args.clone(),
        Duration::from_secs(args.command_timeout_secs),
        args.ton_config_url.clone(),
        args.ton_workers,
        Duration::from_secs(args.ton_batch_timeout_secs),
        Duration::from_secs(args.ton_lookup_timeout_secs),
    )?;

    let validator_apis = clock
        .current_set
        .validators
        .into_iter()
        .take(args.limit.unwrap_or(usize::MAX))
        .collect::<Vec<_>>();
    let resolve_requests = validator_apis
        .iter()
        .map(|validator| ResolveRequest {
            validator_public_key: validator.public_key.clone(),
            adnl_addr: validator.adnl_addr.clone(),
        })
        .collect::<Vec<_>>();
    let resolutions = resolver
        .resolve_many(&clock.chain.id, &resolve_requests)
        .await;

    let validators = validator_apis
        .into_iter()
        .zip(resolutions)
        .map(|(validator, resolution)| ValidatorRecord::from_api(validator, resolution))
        .collect::<Vec<_>>();

    let validators_with_adnl = validators
        .iter()
        .filter(|validator| validator.adnl_addr.is_some())
        .count();
    let resolved_total = validators
        .iter()
        .filter(|validator| validator.resolution.is_resolved())
        .count();

    let output = CollectionOutput {
        schema_version: 1,
        chain: OutputChain::from(clock.chain),
        source_url: format!(
            "{}/api/chains/{}/clock",
            base_url.trim_end_matches('/'),
            args.chain
        ),
        fetched_at: clock.fetched_at,
        generated_at: unix_now(),
        round_id: clock.current_set.round_id,
        round_color: clock.current_set.round_color,
        validators_total: clock.current_set.total,
        validators_main: clock.current_set.main,
        validators_with_adnl,
        resolved_total,
        resolver: ResolverMetadata {
            kind: resolver.kind().as_str().to_owned(),
            attempted_network_resolution: resolver.kind().attempted_network_resolution(),
        },
        validators,
    };

    let needs_geo = args.geo || args.map_output.is_some();
    let mut geo_lookups = 0;
    let geo_by_ip = if needs_geo {
        let mut geo_cache = match args.geo_cache.as_deref() {
            Some(path) => GeoCache::load(path)?,
            None => GeoCache::default(),
        };
        let ips = collect_resolved_ips(&output);
        let geo_client = GeoClient::new(args.geo_endpoint.clone(), args.geo_batch_size);
        geo_lookups = if options.force_geo_refresh {
            geo_client
                .enrich_with_refresh(ips, &mut geo_cache, Some(Duration::ZERO))
                .await?
        } else {
            geo_client.enrich(ips, &mut geo_cache).await?
        };
        if let Some(path) = args.geo_cache.as_deref() {
            geo_cache.save(path)?;
        }
        geo_cache.successful_entries()
    } else {
        Default::default()
    };

    let json = if args.compact {
        serde_json::to_string(&output)?
    } else {
        serde_json::to_string_pretty(&output)?
    };

    if let Some(output_path) = args.output.as_deref() {
        write_json(output_path, &json)?;
    } else if !options.suppress_stdout {
        println!("{json}");
    }

    let mut map_nodes_count = None;
    if let Some(map_output_path) = args.map_output.as_deref() {
        let map_nodes = build_cached_map_nodes(
            &output,
            &geo_by_ip,
            args.map_cache.as_deref(),
            Duration::from_secs(args.map_stale_after_secs),
            output.generated_at,
        )?;
        map_nodes_count = Some(map_nodes.len());
        let map_json = if args.compact {
            serde_json::to_string(&map_nodes)?
        } else {
            serde_json::to_string_pretty(&map_nodes)?
        };
        write_json(map_output_path, &map_json)?;
    }

    Ok(CollectSummary {
        generated_at: output.generated_at,
        validators_total: output.validators_total,
        validators_with_adnl: output.validators_with_adnl,
        resolved_total: output.resolved_total,
        map_nodes: map_nodes_count,
        geo_lookups,
    })
}

fn load_collector_state(path: Option<&Path>) -> Result<CollectorState> {
    let Some(path) = path else {
        return Ok(CollectorState::default());
    };

    if !path.exists() {
        return Ok(CollectorState::default());
    }

    let json = fs::read_to_string(path)
        .with_context(|| format!("failed to read collector state {}", path.display()))?;
    serde_json::from_str(&json)
        .with_context(|| format!("failed to parse collector state {}", path.display()))
}

fn save_collector_state(path: Option<&Path>, state: &CollectorState) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };

    let json = serde_json::to_string_pretty(state)?;
    write_json(path, &json)
}

fn load_config_file(path: &Path) -> Result<AddressResolverConfig> {
    let json = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    serde_json::from_str(&json)
        .with_context(|| format!("failed to parse config {}", path.display()))
}

fn config_base_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn default_map_cache_path(base_dir: &Path, state: Option<&PathBuf>) -> Option<PathBuf> {
    let state = state?;
    let state_path = resolve_config_path(base_dir, state);
    let parent = state_path.parent()?;
    Some(parent.join("ton_map_cache.json"))
}

fn resolve_config_path(base_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_secs()
}

fn write_json(path: &Path, json: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
    }

    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, json)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "failed to rename {} to {}",
            tmp_path.display(),
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_maps_to_loop_args_with_relative_paths() {
        let config: AddressResolverConfig = serde_json::from_str(
            r#"{
                "chain": "ton",
                "interval_secs": 60,
                "full_geo_refresh_secs": 3600,
                "state": "state/ton_nodes_state.json",
                "output": "out/ton_full.json",
                "map_output": "out/ton_nodes.json",
                "map_stale_after_secs": 3600,
                "compact": true,
                "resolver": {
                    "kind": "ton-dht",
                    "command": "tools/ton-dht-resolver/ton-dht-resolver",
                    "ton_workers": 16,
                    "ton_batch_timeout_secs": 600,
                    "ton_lookup_timeout_secs": 30
                },
                "geo": {
                    "cache": "state/ton_geo_cache.json"
                }
            }"#,
        )
        .unwrap();

        let args = config.to_collect_loop_args(Path::new("/srv/address_resolver/config.json"));

        assert_eq!(config.base_url, "https://validatorsclock.xyz");
        assert_eq!(args.collect.chain, "ton");
        assert_eq!(args.collect.resolver, ResolverKind::TonDht);
        assert_eq!(args.collect.ton_workers, 16);
        assert_eq!(
            args.collect.command.unwrap(),
            PathBuf::from("/srv/address_resolver/tools/ton-dht-resolver/ton-dht-resolver")
        );
        assert_eq!(
            args.collect.map_output.unwrap(),
            PathBuf::from("/srv/address_resolver/out/ton_nodes.json")
        );
        assert_eq!(
            args.collect.map_cache.unwrap(),
            PathBuf::from("/srv/address_resolver/state/ton_map_cache.json")
        );
        assert_eq!(args.collect.map_stale_after_secs, 3600);
        assert_eq!(
            args.collect.geo_cache.unwrap(),
            PathBuf::from("/srv/address_resolver/state/ton_geo_cache.json")
        );
        assert_eq!(
            args.state.unwrap(),
            PathBuf::from("/srv/address_resolver/state/ton_nodes_state.json")
        );
    }
}
