use serde::Serialize;

use crate::client::{ChainInfo, ValidatorApi};

#[derive(Debug, Serialize)]
pub struct CollectionOutput {
    pub schema_version: u32,
    pub chain: OutputChain,
    pub source_url: String,
    pub fetched_at: u64,
    pub generated_at: u64,
    pub round_id: u64,
    pub round_color: Option<String>,
    pub validators_total: usize,
    pub validators_main: usize,
    pub validators_with_adnl: usize,
    pub resolved_total: usize,
    pub resolver: ResolverMetadata,
    pub validators: Vec<ValidatorRecord>,
}

#[derive(Debug, Serialize)]
pub struct OutputChain {
    pub id: String,
    pub name: String,
    pub color: String,
    pub token_symbol: String,
    pub rpc_label: Option<String>,
}

impl From<ChainInfo> for OutputChain {
    fn from(value: ChainInfo) -> Self {
        Self {
            id: value.id,
            name: value.name,
            color: value.color,
            token_symbol: value.token_symbol,
            rpc_label: value.rpc_label,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ResolverMetadata {
    pub kind: String,
    pub attempted_network_resolution: bool,
}

#[derive(Debug, Serialize)]
pub struct ValidatorRecord {
    pub validator_public_key: String,
    pub adnl_addr: Option<String>,
    pub wallet: Option<String>,
    pub source_address: Option<String>,
    pub source_contract_type_hash: Option<String>,
    pub contract_type: Option<String>,
    pub stake: Option<String>,
    pub weight: Option<String>,
    pub resolution: Resolution,
}

impl ValidatorRecord {
    pub fn from_api(value: ValidatorApi, resolution: Resolution) -> Self {
        let source_address = value.source.as_ref().map(|source| source.address.clone());
        let source_contract_type_hash = value
            .source
            .as_ref()
            .and_then(|source| source.contract_type_hash.clone());

        Self {
            validator_public_key: value.public_key,
            adnl_addr: value.adnl_addr,
            wallet: value.wallet,
            source_address,
            source_contract_type_hash,
            contract_type: value.contract_type,
            stake: value.stake,
            weight: value.weight,
            resolution,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Resolution {
    pub status: ResolutionStatus,
    pub addresses: Vec<ResolvedAddress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_public_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address_list: Option<AddressListMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<String>,
}

impl Resolution {
    pub fn not_attempted() -> Self {
        Self {
            status: ResolutionStatus::NotAttempted,
            addresses: Vec::new(),
            owner_public_key: None,
            address_list: None,
            error: None,
            raw_output: None,
        }
    }

    pub fn missing_adnl() -> Self {
        Self {
            status: ResolutionStatus::MissingAdnl,
            addresses: Vec::new(),
            owner_public_key: None,
            address_list: None,
            error: Some("validator has no adnl_addr in active set".to_owned()),
            raw_output: None,
        }
    }

    pub fn invalid_adnl(adnl_addr: &str) -> Self {
        Self {
            status: ResolutionStatus::InvalidAdnl,
            addresses: Vec::new(),
            owner_public_key: None,
            address_list: None,
            error: Some(format!("adnl_addr must be 32 bytes hex, got {adnl_addr}")),
            raw_output: None,
        }
    }

    pub fn failed(error: impl Into<String>, raw_output: Option<String>) -> Self {
        Self {
            status: ResolutionStatus::Failed,
            addresses: Vec::new(),
            owner_public_key: None,
            address_list: None,
            error: Some(error.into()),
            raw_output,
        }
    }

    pub fn from_resolver_output(
        addresses: Vec<ResolvedAddress>,
        owner_public_key: Option<String>,
        address_list: Option<AddressListMetadata>,
        raw_output: Option<String>,
    ) -> Self {
        let status = if addresses.is_empty() {
            ResolutionStatus::NotFound
        } else {
            ResolutionStatus::Resolved
        };

        Self {
            status,
            addresses,
            owner_public_key,
            address_list,
            error: None,
            raw_output,
        }
    }

    pub fn is_resolved(&self) -> bool {
        matches!(self.status, ResolutionStatus::Resolved)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionStatus {
    NotAttempted,
    MissingAdnl,
    InvalidAdnl,
    NotFound,
    Resolved,
    Failed,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ResolvedAddress {
    pub ip: String,
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct AddressListMetadata {
    pub version: Option<i64>,
    pub reinit_date: Option<i64>,
    pub priority: Option<i64>,
    pub expire_at: Option<i64>,
}
