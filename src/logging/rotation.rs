use anyhow::{Result, bail, ensure};
use flate2::{Compression, write::GzEncoder};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{fd::AsFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Rotation {
    pub max_bytes: u64,
    pub interval_seconds: u64,
    pub keep: usize,
    pub gzip: bool,
}
impl Rotation {
    pub fn parse(args: &[String]) -> Result<Self> {
        if args == ["off"] {
            return Ok(Self::default());
        }
        ensure!(
            !args.is_empty(),
            "rgnix_log_rotation expects off or size=N interval=TIME keep=N gzip=on|off"
        );
        let mut policy = Self {
            keep: 7,
            ..Self::default()
        };
        let mut seen = BTreeSet::new();
        for arg in args {
            let Some((key, value)) = arg.split_once('=') else {
                bail!("rotation options require key=value");
            };
            ensure!(seen.insert(key), "duplicate rotation option {key}");
            match key {
                "size" => {
                    policy.max_bytes = crate::config::size(value)?;
                    ensure!(
                        (1..=1024 * 1024 * 1024 * 1024).contains(&policy.max_bytes),
                        "rotation size must be 1 byte..1 TiB"
                    );
                }
                "interval" => {
                    let (n, unit) = match value.as_bytes().last() {
                        Some(b's') => (&value[..value.len() - 1], 1),
                        Some(b'm') => (&value[..value.len() - 1], 60),
                        Some(b'h') => (&value[..value.len() - 1], 3600),
                        Some(b'd') => (&value[..value.len() - 1], 86400),
                        _ => (value, 1),
                    };
                    policy.interval_seconds = n
                        .parse::<u64>()?
                        .checked_mul(unit)
                        .ok_or_else(|| anyhow::anyhow!("rotation interval overflow"))?;
                    ensure!(
                        (1..=365 * 86400).contains(&policy.interval_seconds),
                        "rotation interval must be 1s..365d"
                    );
                }
                "keep" => {
                    policy.keep = value.parse()?;
                    ensure!(
                        (1..=1000).contains(&policy.keep),
                        "rotation keep must be 1..1000"
                    );
                }
                "gzip" => {
                    policy.gzip = match value {
                        "on" => true,
                        "off" => false,
                        _ => bail!("rotation gzip must be on or off"),
                    }
                }
                _ => bail!("unsupported rotation option {key}"),
            }
        }
        ensure!(
            policy.max_bytes > 0 || policy.interval_seconds > 0,
            "rotation requires size or interval"
        );
        Ok(policy)
    }
    fn period(&self, time: SystemTime) -> u64 {
        time.duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            / self.interval_seconds.max(1)
    }
    pub(super) fn enabled(&self) -> bool {
        self.max_bytes > 0 || self.interval_seconds > 0
    }
}

pub(super) struct FileSink {
    file: File,
    period: u64,
    policy: Rotation,
}

pub(super) fn open_file(path: &Path) -> io::Result<File> {
    // Inherited stdout/stderr may remain writable after a supervisor drops privileges,
    // even when reopening /proc/self/fd through /dev/std* would fail permission checks.
    match path.to_str() {
        Some("/dev/stdout") => Ok(std::io::stdout().as_fd().try_clone_to_owned()?.into()),
        Some("/dev/stderr") => Ok(std::io::stderr().as_fd().try_clone_to_owned()?.into()),
        _ => OpenOptions::new().create(true).append(true).open(path),
    }
}

impl FileSink {
    pub fn open(path: &Path, policy: &Rotation) -> io::Result<Self> {
        let file = open_file(path)?;
        let modified = file
            .metadata()?
            .modified()
            .unwrap_or_else(|_| SystemTime::now());
        Ok(Self {
            file,
            period: policy.period(modified),
            policy: policy.clone(),
        })
    }

    pub fn write(&mut self, line: &str) -> io::Result<()> {
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")
    }
    pub fn set_policy(&mut self, policy: &Rotation) {
        if &self.policy != policy {
            self.period = policy.period(
                self.file
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or_else(|_| SystemTime::now()),
            );
            self.policy = policy.clone();
        }
    }

    /// Rotate before appending a complete record. Special files and symlinks are never renamed.
    pub fn rotate_if_needed(
        &mut self,
        path: &Path,
        incoming: usize,
    ) -> io::Result<Option<PathBuf>> {
        if !self.policy.enabled() {
            return Ok(None);
        }
        let metadata = self.file.metadata()?;
        let now = self.policy.period(SystemTime::now());
        if !metadata.is_file() {
            return Ok(None);
        }
        if metadata.len() == 0 {
            self.period = now;
            return Ok(None);
        }
        let size_due = self.policy.max_bytes > 0
            && metadata.len().saturating_add(incoming as u64) > self.policy.max_bytes;
        let time_due = self.policy.interval_seconds > 0 && now > self.period;
        if !size_due && !time_due {
            return Ok(None);
        }
        let current = fs::symlink_metadata(path)?;
        if !current.is_file() {
            return Ok(None);
        }
        if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
            // An external rename/create must not archive a file we have never written.
            *self = Self::open(path, &self.policy)?;
            return Ok(None);
        }
        let parent = path.parent().unwrap_or(Path::new("."));
        let replacement = tempfile::NamedTempFile::new_in(parent)?;
        replacement
            .as_file()
            .set_permissions(metadata.permissions())?;
        let writer = OpenOptions::new().append(true).open(replacement.path())?;
        let archives = archives(path)?;
        let mut sequence = archives.last_key_value().map_or(0, |(n, _)| *n);
        let archive = loop {
            sequence = sequence
                .checked_add(1)
                .ok_or_else(|| io::Error::other("log archive sequence exhausted"))?;
            let name = archive_path(path, sequence);
            match fs::hard_link(path, &name) {
                Ok(()) => break name,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        };
        // Preserve the old inode before atomically publishing the new file. If publication
        // fails, the old writer and active pathname still work and no records are truncated.
        if let Err(e) = replacement.persist(path) {
            let _ = fs::remove_file(&archive);
            return Err(e.error);
        }
        self.file = writer;
        self.period = now;
        Ok(Some(archive))
    }
}

fn archive_path(path: &Path, sequence: u64) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".rgnix.{sequence:020}"));
    PathBuf::from(name)
}

fn archives(path: &Path) -> io::Result<BTreeMap<u64, Vec<PathBuf>>> {
    let mut entries: BTreeMap<u64, Vec<PathBuf>> = BTreeMap::new();
    let prefix = format!(
        "{}.rgnix.",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    for item in fs::read_dir(path.parent().unwrap_or(Path::new(".")))? {
        let item = item?;
        let filename = item.file_name();
        let Some(filename) = filename.to_str() else {
            continue;
        };
        let Some(number) = filename.strip_prefix(&prefix) else {
            continue;
        };
        let number = number.strip_suffix(".gz").unwrap_or(number);
        if number.len() != 20
            || !number.bytes().all(|b| b.is_ascii_digit())
            || !item.file_type()?.is_file()
        {
            continue;
        }
        if let Ok(number) = number.parse::<u64>() {
            entries.entry(number).or_default().push(item.path());
        }
    }
    Ok(entries)
}

pub(super) fn compress(path: &Path) -> io::Result<()> {
    let mut input = File::open(path)?;
    let output = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    output
        .as_file()
        .set_permissions(input.metadata()?.permissions())?;
    let mut gzip = GzEncoder::new(output, Compression::fast());
    io::copy(&mut input, &mut gzip)?;
    let output = gzip.finish()?;
    let mut name = path.as_os_str().to_os_string();
    name.push(".gz");
    output
        .persist_noclobber(PathBuf::from(name))
        .map_err(|e| e.error)?;
    fs::remove_file(path)
}

pub(super) fn prune(path: &Path, keep: usize) -> io::Result<()> {
    let archives = archives(path)?;
    for (_, paths) in archives.iter().take(archives.len().saturating_sub(keep)) {
        for archive in paths {
            fs::remove_file(archive)?;
        }
    }
    Ok(())
}
