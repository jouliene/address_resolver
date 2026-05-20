use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    geo::GeoInfo,
    model::{CollectionOutput, ResolvedAddress, ValidatorRecord},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MapNode {
    pub peer: String,
    pub ip: String,
    pub city: Option<String>,
    pub country: Option<String>,
    pub isp: Option<String>,
    pub lat: f64,
    pub lon: f64,
}

pub fn collect_resolved_ips(output: &CollectionOutput) -> Vec<String> {
    output
        .validators
        .iter()
        .flat_map(|validator| validator.resolution.addresses.iter())
        .map(|address| address.ip.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub fn build_map_nodes(
    output: &CollectionOutput,
    geo_by_ip: &BTreeMap<String, GeoInfo>,
) -> Vec<MapNode> {
    let mut nodes = Vec::new();

    for validator in &output.validators {
        if let Some(node) = build_map_node(validator, geo_by_ip) {
            nodes.push(node);
        }
    }

    nodes
}

pub fn build_cached_map_nodes(
    output: &CollectionOutput,
    geo_by_ip: &BTreeMap<String, GeoInfo>,
    cache_path: Option<&Path>,
    stale_after: Duration,
    now: u64,
) -> Result<Vec<MapNode>> {
    let fresh_nodes = build_map_nodes(output, geo_by_ip);
    let Some(cache_path) = cache_path else {
        return Ok(fresh_nodes);
    };

    let mut cache = MapNodeCache::load(cache_path)?;
    let active_peers = output
        .validators
        .iter()
        .map(|validator| validator.validator_public_key.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();

    for node in fresh_nodes {
        cache.entries.insert(
            node.peer.to_ascii_lowercase(),
            MapNodeCacheEntry {
                updated_at: now,
                node,
            },
        );
    }

    let stale_after_secs = stale_after.as_secs();
    cache.entries.retain(|peer, entry| {
        active_peers.contains(peer) && now.saturating_sub(entry.updated_at) <= stale_after_secs
    });

    let nodes = output
        .validators
        .iter()
        .filter_map(|validator| {
            cache
                .entries
                .get(&validator.validator_public_key.to_ascii_lowercase())
                .map(|entry| entry.node.clone())
        })
        .collect::<Vec<_>>();

    cache.save(cache_path)?;
    Ok(nodes)
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct MapNodeCache {
    entries: BTreeMap<String, MapNodeCacheEntry>,
}

impl MapNodeCache {
    fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let json = fs::read_to_string(path)
            .with_context(|| format!("failed to read map cache {}", path.display()))?;
        serde_json::from_str(&json)
            .with_context(|| format!("failed to parse map cache {}", path.display()))
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
        }

        let json = serde_json::to_string_pretty(self)?;
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
}

#[derive(Debug, Deserialize, Serialize)]
struct MapNodeCacheEntry {
    updated_at: u64,
    node: MapNode,
}

fn build_map_node(
    validator: &ValidatorRecord,
    geo_by_ip: &BTreeMap<String, GeoInfo>,
) -> Option<MapNode> {
    let address = choose_canonical_address(&validator.resolution.addresses)?;
    let geo = geo_by_ip.get(&address.ip)?;
    let lat = geo.lat?;
    let lon = geo.lon?;

    Some(MapNode {
        peer: validator.validator_public_key.clone(),
        ip: address.ip.clone(),
        city: geo.city.clone(),
        country: geo.country.clone(),
        isp: geo.isp.clone(),
        lat,
        lon,
    })
}

fn choose_canonical_address(addresses: &[ResolvedAddress]) -> Option<&ResolvedAddress> {
    addresses
        .iter()
        .find(|address| address.port == Some(50000))
        .or_else(|| addresses.first())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::model::{
        CollectionOutput, OutputChain, Resolution, ResolvedAddress, ResolverMetadata,
        ValidatorRecord,
    };

    use super::*;

    #[test]
    fn exports_compatible_map_node() {
        let output = test_output(
            "validator",
            Resolution::from_resolver_output(
                vec![
                    ResolvedAddress {
                        ip: "1.2.3.4".to_owned(),
                        port: Some(30303),
                        transport: Some("udp4".to_owned()),
                    },
                    ResolvedAddress {
                        ip: "1.2.3.4".to_owned(),
                        port: Some(50000),
                        transport: Some("udp4".to_owned()),
                    },
                ],
                Some("owner".to_owned()),
                None,
                None,
            ),
        );
        let geo = test_geo("1.2.3.4");

        let nodes = build_map_nodes(&output, &geo);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].peer, "validator");
        assert_eq!(nodes[0].ip, "1.2.3.4");
        assert_eq!(nodes[0].lat, 1.0);
        let json = serde_json::to_value(&nodes[0]).unwrap();
        assert_eq!(
            json.as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["city", "country", "ip", "isp", "lat", "lon", "peer"]
        );
    }

    #[test]
    fn cached_map_keeps_temporarily_unresolved_validator_until_ttl() {
        let cache_path = temp_cache_path("map-cache");
        let _ = fs::remove_file(&cache_path);

        let resolved_output = test_output(
            "validator",
            Resolution::from_resolver_output(
                vec![ResolvedAddress {
                    ip: "1.2.3.4".to_owned(),
                    port: Some(50000),
                    transport: Some("udp4".to_owned()),
                }],
                Some("owner".to_owned()),
                None,
                None,
            ),
        );
        let nodes = build_cached_map_nodes(
            &resolved_output,
            &test_geo("1.2.3.4"),
            Some(&cache_path),
            Duration::from_secs(3600),
            100,
        )
        .unwrap();
        assert_eq!(nodes.len(), 1);

        let unresolved_output = test_output("validator", Resolution::not_attempted());
        let nodes = build_cached_map_nodes(
            &unresolved_output,
            &BTreeMap::new(),
            Some(&cache_path),
            Duration::from_secs(3600),
            200,
        )
        .unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].ip, "1.2.3.4");

        let nodes = build_cached_map_nodes(
            &unresolved_output,
            &BTreeMap::new(),
            Some(&cache_path),
            Duration::from_secs(3600),
            3701,
        )
        .unwrap();
        assert!(nodes.is_empty());

        let _ = fs::remove_file(cache_path);
    }

    fn test_output(peer: &str, resolution: Resolution) -> CollectionOutput {
        CollectionOutput {
            schema_version: 1,
            chain: OutputChain {
                id: "ton".to_owned(),
                name: "TON".to_owned(),
                color: "#4DB8FF".to_owned(),
                token_symbol: "TON".to_owned(),
                rpc_label: None,
            },
            source_url: "https://example.test".to_owned(),
            fetched_at: 1,
            generated_at: 2,
            round_id: 3,
            round_color: None,
            validators_total: 1,
            validators_main: 1,
            validators_with_adnl: 1,
            resolved_total: 1,
            resolver: ResolverMetadata {
                kind: "test".to_owned(),
                attempted_network_resolution: true,
            },
            validators: vec![ValidatorRecord {
                validator_public_key: peer.to_owned(),
                adnl_addr: Some("adnl".to_owned()),
                wallet: Some("wallet".to_owned()),
                source_address: None,
                source_contract_type_hash: None,
                contract_type: Some("pool".to_owned()),
                stake: None,
                weight: None,
                resolution,
            }],
        }
    }

    fn test_geo(ip: &str) -> BTreeMap<String, GeoInfo> {
        let mut geo = BTreeMap::new();
        geo.insert(
            ip.to_owned(),
            GeoInfo {
                query: ip.to_owned(),
                status: "success".to_owned(),
                updated_at: None,
                message: None,
                country: Some("Wonderland".to_owned()),
                country_code: Some("WL".to_owned()),
                region_name: Some("Region".to_owned()),
                city: Some("City".to_owned()),
                lat: Some(1.0),
                lon: Some(2.0),
                isp: Some("ISP".to_owned()),
                org: None,
                asn: None,
            },
        );
        geo
    }

    fn temp_cache_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "address_resolver_{name}_{}_{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
