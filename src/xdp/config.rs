use anyhow::{Context, Result, ensure};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
};

pub const ABI: u32 = 2;
pub const MAX_RULES: usize = 128;
pub const MAX_SETS: usize = 32;
pub const RATE_CAPACITY: u32 = 65536;
pub const SET_CAPACITY: usize = 16384;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Pass,
    #[default]
    Drop,
    Policy,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Scope {
    pub destinations: Vec<IpNet>,
    pub ports: Vec<u16>,
    pub protocols: Vec<u8>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetEntry {
    pub cidr: IpNet,
    /// Absolute Unix seconds; zero means no expiry. Repeated reloads never extend a ban.
    #[serde(default)]
    pub expires_at: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub revision: String,
    pub policy: Option<PathBuf>,
    pub policy_sha256: Option<String>,
    pub observe: bool,
    pub scope: Scope,
    pub malformed: Action,
    pub unsupported: Action,
    pub fragments: Action,
    pub ceiling_pps: u32,
    pub ceiling_burst: u32,
    pub event_sample_every: u32,
    pub event_max_per_second: u32,
    pub sets: BTreeMap<String, Vec<SetEntry>>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            revision: String::new(),
            policy: None,
            policy_sha256: None,
            observe: false,
            scope: Scope::default(),
            malformed: Action::Drop,
            unsupported: Action::Drop,
            fragments: Action::Policy,
            ceiling_pps: 1_000_000,
            ceiling_burst: 10_000,
            event_sample_every: 0,
            event_max_per_second: 100,
            sets: BTreeMap::new(),
        }
    }
}
impl Config {
    pub fn read(path: Option<&Path>) -> Result<Self> {
        let value: Self = match path {
            Some(path) => serde_json::from_slice(&read_bounded(path, 4 * 1024 * 1024)?)
                .with_context(|| path.display().to_string())?,
            None => Self::default(),
        };
        ensure!(value.version == 1, "unsupported XDP configuration version");
        ensure!(
            value.revision.len() <= 128 && !value.revision.chars().any(char::is_control),
            "invalid revision"
        );
        ensure!(
            value.scope.destinations.len() <= 32
                && value.scope.ports.len() <= 32
                && value.scope.protocols.len() <= 16,
            "scope exceeds 32 destinations/ports or 16 protocols"
        );
        ensure!(
            !value.scope.ports.contains(&0),
            "scope ports must be nonzero"
        );
        ensure!(
            !matches!(value.malformed, Action::Policy)
                && !matches!(value.unsupported, Action::Policy),
            "malformed/unsupported must be pass or drop"
        );
        ensure!(
            (1..=1_000_000_000).contains(&value.ceiling_pps)
                && (1..=1_000_000_000).contains(&value.ceiling_burst),
            "invalid global ceiling"
        );
        ensure!(
            value.event_max_per_second <= 1000,
            "event export is capped at 1000 samples/second"
        );
        ensure!(
            value.sets.len() <= MAX_SETS
                && value.sets.values().map(Vec::len).sum::<usize>() <= SET_CAPACITY,
            "address set capacity exceeded"
        );
        for (name, entries) in &value.sets {
            valid_name(name)?;
            let mut networks = std::collections::BTreeSet::new();
            for entry in entries {
                ensure!(
                    networks.insert(entry.cidr.trunc().to_string()),
                    "duplicate CIDR in set {name}"
                );
                ensure!(entry.expires_at <= 253402300799, "invalid address expiry");
            }
        }
        if let Some(hash) = &value.policy_sha256 {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "policy_sha256 must be 64 hex digits"
            );
        }
        Ok(value)
    }
    pub fn source(&self, source: &Path, config_path: Option<&Path>) -> PathBuf {
        match &self.policy {
            Some(path) if path.is_relative() => config_path
                .and_then(Path::parent)
                .unwrap_or(Path::new("."))
                .join(path),
            Some(path) => path.clone(),
            None => source.to_owned(),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Rule {
    pub name: String,
    pub line: usize,
    pub column: usize,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Metadata {
    pub abi: u32,
    pub compiler: String,
    pub source_sha256: String,
    pub rules: Vec<Rule>,
    pub sets: Vec<String>,
    pub keyed_rates: bool,
    pub rate_ids: Vec<u64>,
    #[serde(default)]
    pub dispatcher_config: Option<Config>,
}
pub fn valid_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
        "names must contain 1..64 ASCII letters, digits, _, . or -"
    );
    Ok(())
}
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn id(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(Sha256::digest(bytes)[..8].try_into().unwrap())
}
pub fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .with_context(|| path.display().to_string())?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "{} exceeds {} bytes",
        path.display(),
        limit
    );
    Ok(bytes)
}
