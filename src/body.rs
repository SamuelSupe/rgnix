use anyhow::{Result, ensure};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

pub const MAX_INSPECT_BYTES: usize = 256 * 1024;
pub const MODE_ANNOTATION: &str = "rgnix.io/request-body";
pub const TIMEOUT_ANNOTATION: &str = "rgnix.io/request-body-timeout";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Inspection {
    #[default]
    Off,
    Full(usize),
    Prefix(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyPolicy {
    pub inspection: Inspection,
    pub timeout: Duration,
}

impl Default for BodyPolicy {
    fn default() -> Self {
        Self {
            inspection: Inspection::Off,
            timeout: Duration::from_secs(5),
        }
    }
}

impl Inspection {
    pub fn parse(value: &str) -> Result<Self> {
        let args: Vec<_> = value.split_whitespace().collect();
        if args == ["off"] {
            return Ok(Self::Off);
        }
        ensure!(
            args.len() == 2,
            "request body expects off, full SIZE or prefix SIZE"
        );
        let limit = crate::config::size(args[1])?;
        ensure!(
            (1..=MAX_INSPECT_BYTES as u64).contains(&limit),
            "request body inspection size must be 1 byte to 256 KiB"
        );
        match args[0] {
            "full" => Ok(Self::Full(limit as usize)),
            "prefix" => Ok(Self::Prefix(limit as usize)),
            _ => anyhow::bail!("request body expects off, full SIZE or prefix SIZE"),
        }
    }
}

impl BodyPolicy {
    pub fn from_annotations(annotations: &BTreeMap<String, String>) -> Result<Self> {
        let mut policy = Self::default();
        if let Some(value) = annotations.get(MODE_ANNOTATION) {
            policy.inspection = Inspection::parse(value)?;
        }
        if let Some(value) = annotations.get(TIMEOUT_ANNOTATION) {
            policy.timeout = Self::parse_timeout(value)?;
        }
        Ok(policy)
    }

    pub fn parse_timeout(value: &str) -> Result<Duration> {
        let timeout = crate::config::duration(value)?;
        ensure!(
            !timeout.is_zero() && timeout <= Duration::from_secs(60),
            "request body inspection timeout must be greater than zero and at most 60s"
        );
        Ok(timeout)
    }

    pub fn validate(&self) -> bool {
        let limit_valid = match self.inspection {
            Inspection::Off => true,
            Inspection::Full(n) | Inspection::Prefix(n) => (1..=MAX_INSPECT_BYTES).contains(&n),
        };
        limit_valid && !self.timeout.is_zero() && self.timeout <= Duration::from_secs(60)
    }
}

#[derive(Clone, Debug)]
pub struct BodyView {
    pub bytes: Bytes,
    pub complete: bool,
}
