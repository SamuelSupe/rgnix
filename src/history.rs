use crate::{config::bundle::Bundle, model::RuntimeSnapshot, script::Compiler};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Serialize, Deserialize)]
struct Revision {
    version: u64,
    bundle: Arc<Bundle>,
}
#[derive(Default, Serialize, Deserialize)]
struct Journal {
    source: PathBuf,
    revisions: Vec<Revision>,
}
pub struct History {
    directory: PathBuf,
    journal: Journal,
    _lock: File,
}
fn read(directory: &Path) -> Result<Option<Journal>> {
    let path = directory.join("history.json");
    if !path.exists() {
        return Ok(None);
    }
    let file = File::open(path)?;
    ensure!(
        file.metadata()?.len() <= 64 * 1024 * 1024,
        "history exceeds 64 MiB"
    );
    let mut bytes = Vec::new();
    file.take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    let journal: Journal = serde_json::from_slice(&bytes)?;
    ensure!(journal.revisions.len() <= 9, "invalid history length");
    Ok(Some(journal))
}
pub fn initial(
    source: &Path,
    directory: Option<&Path>,
    compiler: &Compiler,
) -> Result<RuntimeSnapshot> {
    if let Some(directory) = directory
        && let Some(journal) = read(directory)?
    {
        ensure!(
            journal.source == source,
            "history belongs to another configuration file"
        );
        if let Some(last) = journal.revisions.last() {
            return last.bundle.compile(compiler, last.version);
        }
    }
    // Capture first so the compiled configuration and its durable assets agree.
    Arc::new(Bundle::capture(source)?).compile(compiler, 1)
}
impl History {
    pub fn open(directory: &Path, source: &Path) -> Result<Self> {
        std::fs::create_dir_all(directory)?;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(directory.join("lock"))?;
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "history directory already in use"
        );
        let journal = read(directory)?.unwrap_or_else(|| Journal {
            source: source.into(),
            ..Default::default()
        });
        ensure!(
            journal.source == source,
            "history belongs to another configuration file"
        );
        Ok(Self {
            directory: directory.into(),
            journal,
            _lock: lock,
        })
    }
    pub fn versions(&self) -> Vec<u64> {
        self.journal.revisions.iter().map(|r| r.version).collect()
    }
    pub fn restore(&self, compiler: &Compiler, version: u64) -> Result<RuntimeSnapshot> {
        self.journal
            .revisions
            .iter()
            .find(|r| r.version == version)
            .context("version is not retained")?
            .bundle
            .compile(compiler, version)
    }
    pub fn record(&mut self, snapshot: &RuntimeSnapshot) -> Result<()> {
        let bundle = snapshot
            .source_bundle
            .clone()
            .context("snapshot has no source bundle")?;
        let mut revisions = self.journal.revisions.clone();
        if revisions
            .last()
            .is_some_and(|r| r.version == snapshot.version)
        {
            return Ok(());
        }
        revisions.push(Revision {
            version: snapshot.version,
            bundle,
        });
        while revisions.len() > 9
            || revisions
                .iter()
                .map(|r| r.bundle.assets.values().map(Vec::len).sum::<usize>())
                .sum::<usize>()
                > 32 * 1024 * 1024
            || revisions
                .iter()
                .map(|r| r.bundle.config_bytes())
                .sum::<usize>()
                > 8 * 1024 * 1024
        {
            revisions.remove(0);
        }
        let journal = Journal {
            source: self.journal.source.clone(),
            revisions,
        };
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer(&mut file, &journal)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(self.directory.join("history.json"))?;
        File::open(&self.directory)?.sync_all()?;
        self.journal = journal;
        Ok(())
    }
}
