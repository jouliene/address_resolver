use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct ValidatorsClockClient {
    http: reqwest::Client,
    base_url: String,
}

impl ValidatorsClockClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    pub async fn fetch_clock(&self, chain_id: &str) -> Result<ClockResponse> {
        let url = format!("{}/api/chains/{}/clock", self.base_url, chain_id);
        self.http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("failed to fetch {url}"))?
            .error_for_status()
            .with_context(|| format!("validators clock API returned an error for {url}"))?
            .json::<ClockResponse>()
            .await
            .with_context(|| format!("failed to decode validators clock response from {url}"))
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ClockResponse {
    pub chain: ChainInfo,
    pub fetched_at: u64,
    pub current_set: ValidatorSet,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChainInfo {
    pub id: String,
    pub name: String,
    pub color: String,
    pub token_symbol: String,
    pub rpc_label: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ValidatorSet {
    pub round_id: u64,
    pub round_color: Option<String>,
    pub total: usize,
    pub main: usize,
    pub validators: Vec<ValidatorApi>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ValidatorApi {
    pub public_key: String,
    pub adnl_addr: Option<String>,
    pub wallet: Option<String>,
    pub source: Option<ValidatorSource>,
    pub contract_type: Option<String>,
    pub stake: Option<String>,
    pub weight: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ValidatorSource {
    pub address: String,
    pub contract_type_hash: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clock_response_subset() {
        let json = r##"
        {
          "chain": {
            "id": "ton",
            "name": "TON",
            "color": "#4DB8FF",
            "token_symbol": "TON",
            "rpc_label": "jrpc-ton.broxus.com"
          },
          "fetched_at": 1779257945,
          "current_set": {
            "round_id": 27149,
            "round_color": "green",
            "total": 1,
            "main": 1,
            "validators": [{
              "public_key": "63345c7d7dbcc14f8bce8811cf3fba41981ec0d80d4bfc6c5e089fb82f867a5e",
              "adnl_addr": "ff0feaa19326615e62defde8919a2ff4087b60ba8aede8139f201b75665a0093",
              "wallet": "-1:abc",
              "source": {"address": "0:def", "contract_type_hash": null},
              "contract_type": "ValidatorController",
              "stake": "1.0",
              "weight": "10"
            }]
          }
        }
        "##;

        let parsed: ClockResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.chain.id, "ton");
        assert_eq!(parsed.current_set.validators.len(), 1);
        assert_eq!(
            parsed.current_set.validators[0].adnl_addr.as_deref(),
            Some("ff0feaa19326615e62defde8919a2ff4087b60ba8aede8139f201b75665a0093")
        );
    }
}
