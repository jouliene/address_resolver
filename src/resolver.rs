use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    process::Stdio,
    str::FromStr,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

use crate::model::{AddressListMetadata, Resolution, ResolvedAddress};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ResolverKind {
    None,
    Command,
    TonDht,
}

impl ResolverKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Command => "command",
            Self::TonDht => "ton-dht",
        }
    }

    pub fn attempted_network_resolution(self) -> bool {
        matches!(self, Self::Command | Self::TonDht)
    }
}

#[derive(Clone, Debug)]
pub enum Resolver {
    None,
    Command(CommandResolver),
    TonDht(TonDhtResolver),
}

impl Resolver {
    pub fn kind(&self) -> ResolverKind {
        match self {
            Self::None => ResolverKind::None,
            Self::Command(_) => ResolverKind::Command,
            Self::TonDht(_) => ResolverKind::TonDht,
        }
    }

    pub async fn resolve_many(
        &self,
        chain_id: &str,
        requests: &[ResolveRequest],
    ) -> Vec<Resolution> {
        match self {
            Self::TonDht(resolver) => resolver.resolve_many(requests).await,
            _ => {
                let mut resolutions = Vec::with_capacity(requests.len());
                for request in requests {
                    let resolution = match request.adnl_addr.as_deref() {
                        Some(adnl_addr) => {
                            self.resolve(chain_id, &request.validator_public_key, adnl_addr)
                                .await
                        }
                        None => Resolution::missing_adnl(),
                    };
                    resolutions.push(resolution);
                }
                resolutions
            }
        }
    }

    pub async fn resolve(
        &self,
        chain_id: &str,
        validator_public_key: &str,
        adnl_addr: &str,
    ) -> Resolution {
        if !is_hex_32(adnl_addr) {
            return Resolution::invalid_adnl(adnl_addr);
        }

        match self {
            Self::None => Resolution::not_attempted(),
            Self::Command(resolver) => resolver
                .resolve(chain_id, validator_public_key, adnl_addr)
                .await
                .unwrap_or_else(|err| Resolution::failed(err.to_string(), None)),
            Self::TonDht(_) => Resolution::failed(
                "ton-dht resolver only supports batch resolution through resolve_many",
                None,
            ),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolveRequest {
    pub validator_public_key: String,
    pub adnl_addr: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CommandResolver {
    command: PathBuf,
    args: Vec<String>,
    timeout: Duration,
}

impl CommandResolver {
    pub fn new(command: PathBuf, args: Vec<String>, timeout: Duration) -> Self {
        Self {
            command,
            args,
            timeout,
        }
    }

    async fn resolve(
        &self,
        chain_id: &str,
        validator_public_key: &str,
        adnl_addr: &str,
    ) -> Result<Resolution> {
        let mut command = Command::new(&self.command);
        for arg in self
            .args
            .iter()
            .map(|arg| render_template_arg(arg, chain_id, validator_public_key, adnl_addr))
        {
            command.arg(arg);
        }

        let output = timeout(self.timeout, command.output())
            .await
            .with_context(|| {
                format!(
                    "resolver command timed out after {}s",
                    self.timeout.as_secs()
                )
            })?
            .with_context(|| format!("failed to run {}", self.command.display()))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let raw_output = merge_output(&stdout, &stderr);

        if !output.status.success() {
            return Ok(Resolution::failed(
                format!(
                    "{} exited with status {}",
                    self.command.display(),
                    output.status
                ),
                raw_output,
            ));
        }

        let parsed = parse_command_output(&stdout);
        Ok(Resolution::from_resolver_output(
            parsed.addresses,
            parsed.owner_public_key,
            parsed.address_list,
            raw_output,
        ))
    }
}

#[derive(Clone, Debug)]
pub struct TonDhtResolver {
    command: PathBuf,
    config_url: String,
    workers: usize,
    batch_timeout: Duration,
    per_lookup_timeout: Duration,
}

impl TonDhtResolver {
    pub fn new(
        command: PathBuf,
        config_url: String,
        workers: usize,
        batch_timeout: Duration,
        per_lookup_timeout: Duration,
    ) -> Self {
        Self {
            command,
            config_url,
            workers,
            batch_timeout,
            per_lookup_timeout,
        }
    }

    async fn resolve_many(&self, requests: &[ResolveRequest]) -> Vec<Resolution> {
        let mut output = vec![Resolution::not_attempted(); requests.len()];
        let mut adnl_addrs = Vec::new();
        let mut index_by_adnl = BTreeMap::<String, Vec<usize>>::new();

        for (index, request) in requests.iter().enumerate() {
            let Some(adnl_addr) = request.adnl_addr.as_deref() else {
                output[index] = Resolution::missing_adnl();
                continue;
            };

            if !is_hex_32(adnl_addr) {
                output[index] = Resolution::invalid_adnl(adnl_addr);
                continue;
            }

            if !index_by_adnl.contains_key(adnl_addr) {
                adnl_addrs.push(adnl_addr.to_owned());
            }
            index_by_adnl
                .entry(adnl_addr.to_owned())
                .or_default()
                .push(index);
        }

        if adnl_addrs.is_empty() {
            return output;
        }

        let batch_result = self.run_batch(adnl_addrs).await;
        let batch_output = match batch_result {
            Ok(batch_output) => batch_output,
            Err(err) => {
                let error = err.to_string();
                for indexes in index_by_adnl.values() {
                    for index in indexes {
                        output[*index] = Resolution::failed(error.clone(), None);
                    }
                }
                return output;
            }
        };

        for item in batch_output.results {
            let Some(indexes) = index_by_adnl.get(&item.adnl_addr) else {
                continue;
            };
            let resolution = item.into_resolution();
            for index in indexes {
                output[*index] = resolution.clone();
            }
        }

        output
    }

    async fn run_batch(&self, adnl_addrs: Vec<String>) -> Result<TonDhtBatchOutput> {
        let request = TonDhtBatchRequest { adnl_addrs };
        let request_json = serde_json::to_vec(&request)?;

        let mut child = Command::new(&self.command);
        child
            .arg("--batch")
            .arg("--config-url")
            .arg(&self.config_url)
            .arg("--timeout")
            .arg(format!("{}s", self.batch_timeout.as_secs()))
            .arg("--per-lookup-timeout")
            .arg(format!("{}s", self.per_lookup_timeout.as_secs()))
            .arg("--workers")
            .arg(self.workers.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = child
            .spawn()
            .with_context(|| format!("failed to run {}", self.command.display()))?;
        let mut stdin = child
            .stdin
            .take()
            .context("failed to open ton-dht resolver stdin")?;
        stdin.write_all(&request_json).await?;
        drop(stdin);

        let output = timeout(self.batch_timeout, child.wait_with_output())
            .await
            .with_context(|| {
                format!(
                    "ton-dht resolver timed out after {}s",
                    self.batch_timeout.as_secs()
                )
            })?
            .with_context(|| format!("failed to wait for {}", self.command.display()))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            return Err(anyhow!(
                "{} exited with status {}: {}",
                self.command.display(),
                output.status,
                merge_output(&stdout, &stderr).unwrap_or_default()
            ));
        }

        serde_json::from_str::<TonDhtBatchOutput>(&stdout)
            .with_context(|| format!("failed to parse ton-dht resolver output: {stdout}"))
    }
}

#[derive(Debug, Serialize)]
struct TonDhtBatchRequest {
    adnl_addrs: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TonDhtBatchOutput {
    results: Vec<TonDhtResult>,
}

#[derive(Debug, Deserialize)]
struct TonDhtResult {
    adnl_addr: String,
    owner_public_key: Option<String>,
    addresses: Option<Vec<TonDhtAddress>>,
    version: Option<i64>,
    reinit_date: Option<i64>,
    priority: Option<i64>,
    expire_at: Option<i64>,
    error: Option<String>,
}

impl TonDhtResult {
    fn into_resolution(self) -> Resolution {
        if let Some(error) = self.error.filter(|error| !error.is_empty()) {
            return Resolution::failed(error, None);
        }

        let address_list = if self.version.is_some()
            || self.reinit_date.is_some()
            || self.priority.is_some()
            || self.expire_at.is_some()
        {
            Some(AddressListMetadata {
                version: self.version,
                reinit_date: self.reinit_date,
                priority: self.priority,
                expire_at: self.expire_at,
            })
        } else {
            None
        };

        Resolution::from_resolver_output(
            self.addresses
                .unwrap_or_default()
                .into_iter()
                .filter_map(TonDhtAddress::into_resolved_address)
                .collect(),
            self.owner_public_key,
            address_list,
            None,
        )
    }
}

#[derive(Debug, Deserialize)]
struct TonDhtAddress {
    ip: String,
    port: Option<i64>,
    version: Option<String>,
}

impl TonDhtAddress {
    fn into_resolved_address(self) -> Option<ResolvedAddress> {
        if IpAddr::from_str(&self.ip).is_err() {
            return None;
        }

        Some(ResolvedAddress {
            ip: self.ip,
            port: self.port.and_then(|port| u16::try_from(port).ok()),
            transport: self.version,
        })
    }
}

fn render_template_arg(
    arg: &str,
    chain_id: &str,
    validator_public_key: &str,
    adnl_addr: &str,
) -> String {
    arg.replace("{chain_id}", chain_id)
        .replace("{validator_public_key}", validator_public_key)
        .replace("{adnl_addr}", adnl_addr)
}

fn merge_output(stdout: &str, stderr: &str) -> Option<String> {
    let mut merged = String::new();
    if !stdout.trim().is_empty() {
        merged.push_str(stdout.trim());
    }
    if !stderr.trim().is_empty() {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str(stderr.trim());
    }

    if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}

pub fn is_hex_32(value: &str) -> bool {
    value.len() == 64 && hex::decode(value).is_ok_and(|bytes| bytes.len() == 32)
}

#[cfg(test)]
fn parse_resolved_addresses(stdout: &str) -> Vec<ResolvedAddress> {
    parse_command_output(stdout).addresses
}

#[derive(Debug)]
struct ParsedCommandOutput {
    addresses: Vec<ResolvedAddress>,
    owner_public_key: Option<String>,
    address_list: Option<AddressListMetadata>,
}

fn parse_command_output(stdout: &str) -> ParsedCommandOutput {
    let mut addresses = BTreeSet::new();
    let mut owner_public_key = None;
    let mut address_list = None;

    if let Ok(json) = serde_json::from_str::<Value>(stdout) {
        owner_public_key = json
            .get("owner_public_key")
            .and_then(Value::as_str)
            .map(str::to_owned);
        address_list = parse_address_list_metadata(&json);
        collect_json_addresses(&json, &mut addresses);
    }

    for token in stdout.split_whitespace() {
        let token = token
            .trim_matches(|ch: char| matches!(ch, ',' | ';' | '(' | ')' | '{' | '}' | '"' | '\''));

        if let Some(address) = parse_socket_or_ip(token) {
            addresses.insert(address);
        }
    }

    ParsedCommandOutput {
        addresses: addresses.into_iter().collect(),
        owner_public_key,
        address_list,
    }
}

fn parse_address_list_metadata(json: &Value) -> Option<AddressListMetadata> {
    let object = json.as_object()?;
    let metadata = AddressListMetadata {
        version: object.get("version").and_then(Value::as_i64),
        reinit_date: object.get("reinit_date").and_then(Value::as_i64),
        priority: object.get("priority").and_then(Value::as_i64),
        expire_at: object.get("expire_at").and_then(Value::as_i64),
    };

    if metadata.version.is_some()
        || metadata.reinit_date.is_some()
        || metadata.priority.is_some()
        || metadata.expire_at.is_some()
    {
        Some(metadata)
    } else {
        None
    }
}

fn collect_json_addresses(value: &Value, addresses: &mut BTreeSet<ResolvedAddress>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_json_addresses(item, addresses);
            }
        }
        Value::Object(object) => {
            if let Some(address) = object
                .get("address")
                .or_else(|| object.get("addr"))
                .or_else(|| object.get("socket_addr"))
                .and_then(Value::as_str)
                .and_then(parse_socket_or_ip)
            {
                addresses.insert(address);
            }

            if let Some(ip) = object.get("ip").and_then(Value::as_str) {
                let port = object
                    .get("port")
                    .and_then(Value::as_u64)
                    .and_then(|port| u16::try_from(port).ok());
                let transport = object
                    .get("transport")
                    .or_else(|| object.get("version"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if IpAddr::from_str(ip).is_ok() {
                    addresses.insert(ResolvedAddress {
                        ip: ip.to_owned(),
                        port,
                        transport,
                    });
                }
            }

            for (key, item) in object {
                if matches!(
                    key.as_str(),
                    "address" | "addr" | "socket_addr" | "ip" | "port"
                ) {
                    continue;
                }
                collect_json_addresses(item, addresses);
            }
        }
        Value::String(text) => {
            if let Some(address) = parse_socket_or_ip(text) {
                addresses.insert(address);
            }
        }
        _ => {}
    }
}

fn parse_socket_or_ip(value: &str) -> Option<ResolvedAddress> {
    if let Ok(socket_addr) = SocketAddr::from_str(value) {
        return Some(ResolvedAddress {
            ip: socket_addr.ip().to_string(),
            port: Some(socket_addr.port()),
            transport: None,
        });
    }

    if let Ok(ip_addr) = IpAddr::from_str(value) {
        return Some(ResolvedAddress {
            ip: ip_addr.to_string(),
            port: None,
            transport: None,
        });
    }

    None
}

pub fn build_resolver(
    kind: ResolverKind,
    command: Option<PathBuf>,
    args: Vec<String>,
    timeout: Duration,
    ton_config_url: String,
    ton_workers: usize,
    ton_batch_timeout: Duration,
    ton_lookup_timeout: Duration,
) -> Result<Resolver> {
    match kind {
        ResolverKind::None => Ok(Resolver::None),
        ResolverKind::Command => {
            let command = command
                .ok_or_else(|| anyhow!("--resolver command requires --command <path-or-binary>"))?;
            Ok(Resolver::Command(CommandResolver::new(
                command, args, timeout,
            )))
        }
        ResolverKind::TonDht => {
            let command = command
                .unwrap_or_else(|| PathBuf::from("./tools/ton-dht-resolver/ton-dht-resolver"));
            Ok(Resolver::TonDht(TonDhtResolver::new(
                command,
                ton_config_url,
                ton_workers,
                ton_batch_timeout,
                ton_lookup_timeout,
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_adnl_hex() {
        assert!(is_hex_32(
            "ff0feaa19326615e62defde8919a2ff4087b60ba8aede8139f201b75665a0093"
        ));
        assert!(!is_hex_32("ff0f"));
        assert!(!is_hex_32(
            "zz0feaa19326615e62defde8919a2ff4087b60ba8aede8139f201b75665a0093"
        ));
    }

    #[test]
    fn parses_plain_addresses() {
        let addresses = parse_resolved_addresses("Found 1.2.3.4:30303 / key\n[::1]:30303");
        assert_eq!(
            addresses,
            vec![
                ResolvedAddress {
                    ip: "1.2.3.4".to_owned(),
                    port: Some(30303),
                    transport: None
                },
                ResolvedAddress {
                    ip: "::1".to_owned(),
                    port: Some(30303),
                    transport: None
                }
            ]
        );
    }

    #[test]
    fn parses_json_addresses() {
        let addresses = parse_resolved_addresses(
            r#"[{"ip":"1.2.3.4","port":30303},{"address":"5.6.7.8:30304"}]"#,
        );
        assert_eq!(
            addresses,
            vec![
                ResolvedAddress {
                    ip: "1.2.3.4".to_owned(),
                    port: Some(30303),
                    transport: None
                },
                ResolvedAddress {
                    ip: "5.6.7.8".to_owned(),
                    port: Some(30304),
                    transport: None
                }
            ]
        );
    }

    #[test]
    fn parses_command_metadata() {
        let parsed = parse_command_output(
            r#"{"owner_public_key":"abc","addresses":[{"ip":"1.2.3.4","port":30303,"version":"udp4"}],"version":10,"expire_at":20}"#,
        );

        assert_eq!(parsed.owner_public_key.as_deref(), Some("abc"));
        assert_eq!(parsed.address_list.unwrap().expire_at, Some(20));
        assert_eq!(
            parsed.addresses,
            vec![ResolvedAddress {
                ip: "1.2.3.4".to_owned(),
                port: Some(30303),
                transport: Some("udp4".to_owned())
            }]
        );
    }
}
