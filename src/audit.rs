use anyhow::Result;
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::Mutex,
};

pub struct Audit {
    file: Mutex<File>,
    durable: bool,
}
impl Audit {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        let durable = file.metadata()?.is_file();
        Ok(Self {
            file: Mutex::new(file),
            durable,
        })
    }
    pub fn record(&self, actor: &str, operation: &str, version: u64, result: &str) -> Result<()> {
        let line = serde_json::json!({"timestamp":k8s_openapi::chrono::Utc::now().to_rfc3339(),"actor":actor,"operation":operation,"version":version,"result":result});
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        writeln!(file, "{line}")?;
        file.flush()?;
        if self.durable {
            file.sync_data()?;
        }
        Ok(())
    }
}
