use anyhow::{Result, ensure};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Engine {
    Hyper,
    Pingora,
}

// Keep the rollout default until the release artifact and long-running gates pass.
pub const DEFAULT_ENGINE: Engine = Engine::Pingora;

impl Engine {
    pub fn name(self) -> &'static str {
        match self {
            Self::Hyper => "hyper",
            Self::Pingora => "pingora",
        }
    }
}

#[derive(Clone, Default, clap::Args)]
pub struct EngineOptions {
    /// HTTP engine; overrides the legacy experimental flag and its environment variable.
    #[arg(long, env = "RGNIX_ENGINE")]
    pub engine: Option<Engine>,
    /// Deprecated: use --engine. A false value explicitly selects Pingora.
    #[arg(long, env = "RGNIX_EXPERIMENTAL_HYPER", num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub experimental_hyper: Option<bool>,
}

impl EngineOptions {
    pub fn resolve(&self) -> Result<Engine> {
        let engine = self.engine.unwrap_or(match self.experimental_hyper {
            Some(true) => Engine::Hyper,
            Some(false) => Engine::Pingora,
            None => DEFAULT_ENGINE,
        });
        ensure!(
            engine != Engine::Hyper || cfg!(feature = "hyper-experimental"),
            "Hyper is unavailable in this build; rebuild with the hyper-experimental feature or select --engine pingora"
        );
        Ok(engine)
    }
}
