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
use map_export::{build_map_nodes, collect_resolved_ips};
use model::{CollectionOutput, OutputChain, ResolverMetadata, ValidatorRecord};
use resolver::{ResolveRequest, ResolverKind, build_resolver};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

const DEFAULT_GEO_ENDPOINT: &str = "http://ip-api.com/batch?fields=status,message,country,countryCode,regionName,city,lat,lon,isp,org,as,query";

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
        let map_nodes = build_map_nodes(&output, &geo_by_ip);
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
