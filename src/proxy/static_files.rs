use crate::model::Settings;
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use std::{fs::File, io, path::Path, time::SystemTime};

pub struct StaticFile {
    pub file: File,
    pub length: u64,
    pub mime: String,
    pub etag: String,
    pub modified: SystemTime,
}
pub enum Opened {
    File(StaticFile),
    Directory,
    NotFound,
    Forbidden,
}

pub fn open(settings: &Settings, path: &str) -> io::Result<Opened> {
    let root = match Dir::open_ambient_dir(&settings.root, cap_std::ambient_authority()) {
        Ok(root) => root,
        Err(e) => return classify(e),
    };
    let relative = path.trim_start_matches('/');
    let relative = if relative.is_empty() { "." } else { relative };
    let mut file = match open_readonly(&root, Path::new(relative)) {
        Ok(file) => file,
        Err(e) => return classify(e),
    };
    if file.metadata()?.is_dir() {
        if !path.ends_with('/') {
            return Ok(Opened::Directory);
        }
        let mut found = None;
        for index in &settings.index {
            let name = Path::new(relative).join(index);
            match open_readonly(&root, &name) {
                Ok(f) if f.metadata()?.is_file() => {
                    found = Some((f, index.clone()));
                    break;
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return classify(e),
            }
        }
        let Some((f, name)) = found else {
            return Ok(Opened::Forbidden);
        };
        file = f;
        return describe(file, &name, settings);
    }
    if !file.metadata()?.is_file() {
        return Ok(Opened::Forbidden);
    }
    describe(file, relative, settings)
}

fn open_readonly(root: &Dir, path: &Path) -> io::Result<File> {
    let kind = root.metadata(path)?.file_type();
    if !kind.is_file() && !kind.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "special files cannot be served",
        ));
    }
    // A file can become a FIFO after metadata is read. Avoid waiting for a writer;
    // the caller also checks the opened file's type before reading its contents.
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
    root.open_with(path, &options).map(|file| file.into_std())
}

fn classify(e: io::Error) -> io::Result<Opened> {
    match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => Ok(Opened::NotFound),
        io::ErrorKind::PermissionDenied => Ok(Opened::Forbidden),
        _ => Err(e),
    }
}
fn describe(file: File, name: &str, settings: &Settings) -> io::Result<Opened> {
    let metadata = file.metadata()?;
    let modified = metadata.modified()?;
    let stamp = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let etag = format!(
        "\"{:x}-{:x}-{:x}\"",
        metadata.len(),
        stamp.as_secs(),
        stamp.subsec_nanos()
    );
    let extension = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = settings
        .mime
        .get(&extension)
        .unwrap_or(&settings.default_type)
        .clone();
    Ok(Opened::File(StaticFile {
        file,
        length: metadata.len(),
        mime,
        etag,
        modified,
    }))
}

pub fn range(value: &str, length: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(value) = value.strip_prefix("bytes=") else {
        return Ok(None);
    };
    if value.contains(',') {
        return Ok(None);
    };
    let Some((first, last)) = value.split_once('-') else {
        return Ok(None);
    };
    if length == 0 {
        return Err(());
    };
    if first.is_empty() {
        let suffix = last.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        return Ok(Some((length.saturating_sub(suffix), length - 1)));
    }
    let start = first.parse::<u64>().map_err(|_| ())?;
    let end = if last.is_empty() {
        length - 1
    } else {
        last.parse::<u64>().map_err(|_| ())?.min(length - 1)
    };
    if start >= length || end < start {
        return Err(());
    }
    Ok(Some((start, end)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capability_root_blocks_symlink_escape_and_ranges_are_bounded() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let outside = tempfile::NamedTempFile::new()?;
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape"))?;
        let settings = Settings {
            root: dir.path().into(),
            ..Settings::default()
        };
        assert!(matches!(
            open(&settings, "/escape"),
            Ok(Opened::Forbidden) | Err(_)
        ));
        assert_eq!(range("bytes=-5", 3), Ok(Some((0, 2))));
        assert_eq!(range("bytes=3-", 3), Err(()));
        Ok(())
    }
}
