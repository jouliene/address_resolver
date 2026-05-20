use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::{
    geo::GeoInfo,
    model::{CollectionOutput, ResolvedAddress, ValidatorRecord},
};

#[derive(Debug, Serialize)]
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
    use crate::model::{
        CollectionOutput, OutputChain, Resolution, ResolvedAddress, ResolverMetadata,
        ValidatorRecord,
    };

    use super::*;

    #[test]
    fn exports_compatible_map_node() {
        let output = CollectionOutput {
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
                validator_public_key: "validator".to_owned(),
                adnl_addr: Some("adnl".to_owned()),
                wallet: Some("wallet".to_owned()),
                source_address: None,
                source_contract_type_hash: None,
                contract_type: Some("pool".to_owned()),
                stake: None,
                weight: None,
                resolution: Resolution::from_resolver_output(
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
            }],
        };
        let mut geo = BTreeMap::new();
        geo.insert(
            "1.2.3.4".to_owned(),
            GeoInfo {
                query: "1.2.3.4".to_owned(),
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
}
