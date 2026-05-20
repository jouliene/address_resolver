use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_BATCH_SIZE: usize = 100;

#[derive(Clone, Debug)]
pub struct GeoClient {
    http: reqwest::Client,
    endpoint: String,
    batch_size: usize,
}

impl GeoClient {
    pub fn new(endpoint: impl Into<String>, batch_size: usize) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: endpoint.into(),
            batch_size: batch_size.clamp(1, DEFAULT_BATCH_SIZE),
        }
    }

    pub async fn enrich(
        &self,
        ips: impl IntoIterator<Item = String>,
        cache: &mut GeoCache,
    ) -> Result<usize> {
        self.enrich_with_refresh(ips, cache, None).await
    }

    pub async fn enrich_with_refresh(
        &self,
        ips: impl IntoIterator<Item = String>,
        cache: &mut GeoCache,
        refresh_after: Option<Duration>,
    ) -> Result<usize> {
        let now = unix_now();
        let lookup_ips = ips
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|ip| cache.needs_lookup(ip, refresh_after, now))
            .collect::<Vec<_>>();
        let lookup_count = lookup_ips.len();

        for chunk in lookup_ips.chunks(self.batch_size) {
            let lookups = chunk
                .iter()
                .map(|ip| GeoQuery { query: ip.clone() })
                .collect::<Vec<_>>();
            let records = self
                .http
                .post(&self.endpoint)
                .json(&lookups)
                .send()
                .await
                .with_context(|| format!("failed to call geo endpoint {}", self.endpoint))?
                .error_for_status()
                .with_context(|| format!("geo endpoint returned an error: {}", self.endpoint))?
                .json::<Vec<GeoInfo>>()
                .await
                .context("failed to decode geo response")?;

            for mut record in records {
                record.updated_at = Some(now);
                cache.entries.insert(record.query.clone(), record);
            }
        }

        Ok(lookup_count)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct GeoCache {
    pub entries: BTreeMap<String, GeoInfo>,
}

impl GeoCache {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let json = fs::read_to_string(path)
            .with_context(|| format!("failed to read geo cache {}", path.display()))?;
        serde_json::from_str(&json)
            .with_context(|| format!("failed to parse geo cache {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
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

    pub fn successful_entries(&self) -> BTreeMap<String, GeoInfo> {
        self.entries
            .iter()
            .filter(|(_, info)| info.is_success())
            .map(|(ip, info)| (ip.clone(), info.clone()))
            .collect()
    }

    fn needs_lookup(&self, ip: &str, refresh_after: Option<Duration>, now: u64) -> bool {
        let Some(entry) = self.entries.get(ip) else {
            return true;
        };

        let Some(refresh_after) = refresh_after else {
            return false;
        };

        let Some(updated_at) = entry.updated_at else {
            return true;
        };

        now.saturating_sub(updated_at) >= refresh_after.as_secs()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GeoInfo {
    pub query: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(rename = "countryCode", skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    #[serde(rename = "regionName", skip_serializing_if = "Option::is_none")]
    pub region_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(rename = "as", skip_serializing_if = "Option::is_none")]
    pub asn: Option<String>,
}

impl GeoInfo {
    pub fn is_success(&self) -> bool {
        self.status == "success" && self.lat.is_some() && self.lon.is_some()
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_secs()
}

#[derive(Debug, Serialize)]
struct GeoQuery {
    query: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_successful_entries() {
        let mut cache = GeoCache::default();
        cache.entries.insert(
            "1.2.3.4".to_owned(),
            GeoInfo {
                query: "1.2.3.4".to_owned(),
                status: "success".to_owned(),
                updated_at: None,
                message: None,
                country: Some("Wonderland".to_owned()),
                country_code: Some("WL".to_owned()),
                region_name: None,
                city: Some("Rabbit Hole".to_owned()),
                lat: Some(1.0),
                lon: Some(2.0),
                isp: None,
                org: None,
                asn: None,
            },
        );
        cache.entries.insert(
            "5.6.7.8".to_owned(),
            GeoInfo {
                query: "5.6.7.8".to_owned(),
                status: "fail".to_owned(),
                updated_at: None,
                message: Some("reserved range".to_owned()),
                country: None,
                country_code: None,
                region_name: None,
                city: None,
                lat: None,
                lon: None,
                isp: None,
                org: None,
                asn: None,
            },
        );

        assert_eq!(cache.successful_entries().len(), 1);
    }

    #[test]
    fn refreshes_stale_entries_only_when_requested() {
        let mut cache = GeoCache::default();
        cache.entries.insert(
            "1.2.3.4".to_owned(),
            GeoInfo {
                query: "1.2.3.4".to_owned(),
                status: "success".to_owned(),
                updated_at: Some(100),
                message: None,
                country: None,
                country_code: None,
                region_name: None,
                city: None,
                lat: Some(1.0),
                lon: Some(2.0),
                isp: None,
                org: None,
                asn: None,
            },
        );

        assert!(!cache.needs_lookup("1.2.3.4", None, 200));
        assert!(!cache.needs_lookup("1.2.3.4", Some(Duration::from_secs(200)), 200));
        assert!(cache.needs_lookup("1.2.3.4", Some(Duration::from_secs(50)), 200));
        assert!(cache.needs_lookup("5.6.7.8", None, 200));
    }
}
