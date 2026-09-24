use anyhow::Result;
use arc_swap::ArcSwapOption;
use log::{LevelFilter, Log, Metadata, Record};
use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    sync::{Arc, Mutex, OnceLock},
};

pub struct ErrorOutput {
    level: LevelFilter,
    writer: Mutex<Box<dyn Write + Send>>,
}
impl ErrorOutput {
    pub fn open(path: &Path, level: &str) -> Result<Arc<Self>> {
        let level = level.parse::<LevelFilter>()?;
        let writer: Box<dyn Write + Send> =
            if path == Path::new("stderr") || path == Path::new("/dev/stderr") {
                Box::new(std::io::stderr())
            } else {
                Box::new(OpenOptions::new().create(true).append(true).open(path)?)
            };
        Ok(Arc::new(Self {
            level,
            writer: Mutex::new(writer),
        }))
    }
}
struct SwitchLogger {
    default: env_logger::Logger,
    output: ArcSwapOption<ErrorOutput>,
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
        if let Some(output) = self.output.load_full() {
            if record.level() <= output.level
                && let Ok(mut writer) = output.writer.lock()
            {
                let _ = writeln!(
                    writer,
                    "{} [{}] {}: {}",
                    k8s_openapi::chrono::Utc::now().to_rfc3339(),
                    record.level(),
                    record.target(),
                    record.args()
                );
            }
        } else {
            self.default.log(record);
        }
    }
    fn flush(&self) {
        if let Some(output) = self.output.load_full()
            && let Ok(mut writer) = output.writer.lock()
        {
            let _ = writer.flush();
        }
    }
}
pub fn init() -> Result<()> {
    let logger = LOGGER.get_or_init(|| SwitchLogger {
        default: env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or("info"),
        )
        .build(),
        output: ArcSwapOption::empty(),
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
