use super::config::{self, Config, Metadata};
use anyhow::{Context, Result, ensure};
use object::{Object, ObjectSection};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub struct Candidate {
    pub object: Arc<Vec<u8>>,
    pub metadata: Metadata,
    pub config: Config,
    pub digest: String,
    pub object_digest: String,
    pub input_digest: String,
}
pub fn metadata(object: &[u8]) -> Result<Metadata> {
    let file = object::File::parse(object)?;
    let section = file
        .section_by_name("rgnix_meta")
        .context("object lacks rgnix metadata; recompile using this rgnix version")?;
    let bytes = section.data()?;
    let value: Metadata = serde_json::from_slice(bytes.strip_suffix(&[0]).unwrap_or(bytes))?;
    ensure!(
        value.abi == config::ABI,
        "unsupported XDP object ABI {}",
        value.abi
    );
    ensure!(
        value.rules.len() <= config::MAX_RULES
            && value.sets.len() <= config::MAX_SETS
            && value.rate_ids.len() <= 64,
        "invalid XDP metadata limits"
    );
    for name in value.rules.iter().map(|r| &r.name).chain(value.sets.iter()) {
        config::valid_name(name)?;
    }
    Ok(value)
}
pub struct Inputs {
    pub config: Config,
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub digest: String,
}
impl Inputs {
    pub fn read(source: &Path, config_path: Option<&Path>) -> Result<Self> {
        let config = Config::read(config_path)?;
        let path = config.source(source, config_path);
        let bytes = config::read_bounded(&path, 4 * 1024 * 1024)?;
        let mut fingerprint = serde_json::to_vec(&config)?;
        fingerprint.extend_from_slice(&bytes);
        let digest = config::digest(&fingerprint);
        Ok(Self {
            config,
            path,
            bytes,
            digest,
        })
    }
    pub fn prepare(self, clang: &Path, cached: Option<&Candidate>) -> Result<Candidate> {
        let input_hash = config::digest(&self.bytes);
        let object = if self.path.extension().is_some_and(|s| s == "o") {
            Arc::new(self.bytes)
        } else if let Some(old) = cached.filter(|old| old.metadata.source_sha256 == input_hash) {
            old.object.clone()
        } else {
            Arc::new(
                super::compiler::compile(std::str::from_utf8(&self.bytes)?, clang)
                    .with_context(|| self.path.display().to_string())?,
            )
        };
        let object_digest = config::digest(&object);
        if let Some(expected) = &self.config.policy_sha256 {
            ensure!(
                expected.eq_ignore_ascii_case(&object_digest),
                "object digest does not match policy_sha256"
            );
        }
        let metadata = metadata(&object)?;
        let mut effective_config = self.config;
        if let Some(embedded) = &metadata.dispatcher_config {
            ensure!(
                serde_json::to_value(&effective_config)?
                    == serde_json::to_value(Config::default())?
                    || serde_json::to_value(&effective_config)? == serde_json::to_value(embedded)?,
                "dispatcher configuration is immutable; recompile to change it"
            );
            effective_config = embedded.clone();
        }
        for set in &metadata.sets {
            ensure!(
                effective_config.sets.contains_key(set),
                "missing address set {set}"
            );
        }
        let mut semantic = effective_config.clone();
        semantic.policy = None;
        semantic.policy_sha256 = None;
        let mut fingerprint = serde_json::to_vec(&semantic)?;
        fingerprint.extend_from_slice(&object);
        Ok(Candidate {
            digest: config::digest(&fingerprint),
            object,
            metadata,
            config: effective_config,
            object_digest,
            input_digest: self.digest,
        })
    }
}
