pub mod access;
mod files;
mod rotation;
pub use files::FileLogs;
pub use rotation::Rotation;

use anyhow::Result;
use arc_swap::ArcSwapOption;
use log::{LevelFilter, Log, Metadata, Record};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

pub struct ErrorOutput {
    level: LevelFilter,
    path: PathBuf,
}
impl ErrorOutput {
    pub fn open(path: &Path, level: &str) -> Result<Arc<Self>> {
        let level = level.parse::<LevelFilter>()?;
        let path = if path == Path::new("stderr") {
            Path::new("/dev/stderr")
        } else {
            path
        };
        rotation::open_file(path)?;
        Ok(Arc::new(Self {
            level,
            path: path.into(),
        }))
    }
}
struct SwitchLogger {
    default: env_logger::Logger,
    output: ArcSwapOption<ErrorOutput>,
    files: ArcSwapOption<FileLogs>,
}
static LOGGER: OnceLock<SwitchLogger> = OnceLock::new();
impl Log for SwitchLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.output.load().as_ref().map_or_else(
            || self.default.enabled(metadata),
            |o| metadata.level() <= o.level,
        )
    }
    fn log(&self, record: &Record) {
        if let Some(output) = self.output.load_full()
            && let Some(files) = self.files.load_full()
        {
            if record.level() <= output.level {
                let line = format!(
                    "{} [{}] {}: {}",
                    k8s_openapi::chrono::Utc::now().to_rfc3339(),
                    record.level(),
                    record.target(),
                    record.args()
                );
                files.write(output.path.clone(), line, files::Kind::Error);
            }
        } else {
            self.default.log(record);
        }
    }
    fn flush(&self) {
        self.default.flush();
    }
}
pub fn init() -> Result<()> {
    let logger = LOGGER.get_or_init(|| SwitchLogger {
        default: env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or("info"),
        )
        .build(),
        output: ArcSwapOption::empty(),
        files: ArcSwapOption::empty(),
    });
    log::set_logger(logger)?;
    log::set_max_level(LevelFilter::Trace);
    Ok(())
}
pub fn configure(output: Option<Arc<ErrorOutput>>) {
    if let Some(logger) = LOGGER.get() {
        logger.output.store(output);
    }
}
pub fn install_files(files: Arc<FileLogs>) {
    if let Some(logger) = LOGGER.get() {
        logger.files.store(Some(files));
    }
}
