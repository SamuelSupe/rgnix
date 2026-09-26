use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Timeouts {
    pub request: Option<String>,
    pub backend_request: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Budgets {
    pub request: Option<Duration>,
    pub backend_request: Option<Duration>,
}

impl Timeouts {
    pub fn budgets(&self) -> Result<Budgets> {
        let request = self.request.as_deref().map(duration).transpose()?;
        let backend = self.backend_request.as_deref().map(duration).transpose()?;
        if let (Some(request), Some(backend)) = (request, backend) {
            ensure!(
                request.is_zero() || (!backend.is_zero() && backend <= request),
                "backendRequest timeout must not exceed request timeout"
            );
        }
        Ok(Budgets {
            request: request.filter(|v| !v.is_zero()),
            backend_request: backend.filter(|v| !v.is_zero()),
        })
    }
}

fn duration(mut text: &str) -> Result<Duration> {
    let mut millis = 0u64;
    let mut components = 0;
    while !text.is_empty() {
        let digits = text.bytes().take_while(u8::is_ascii_digit).count();
        ensure!(
            (1..=5).contains(&digits) && components < 4,
            "invalid Gateway duration"
        );
        let value = text[..digits].parse::<u64>()?;
        text = &text[digits..];
        let (unit, multiplier) = [("ms", 1), ("h", 3_600_000), ("m", 60_000), ("s", 1_000)]
            .into_iter()
            .find(|(unit, _)| text.starts_with(unit))
            .context("Gateway duration requires h, m, s or ms")?;
        millis += value * multiplier;
        text = &text[unit.len()..];
        components += 1;
    }
    ensure!(components > 0, "empty Gateway duration");
    Ok(Duration::from_millis(millis))
}
