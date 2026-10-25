use crate::model::Settings;
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use std::{
    borrow::Cow,
    fs::{File, Metadata},
    io,
    path::Path,
    time::SystemTime,
};

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
    Status(u16),
}

pub fn open_route(settings: &Settings, path: &str, prefix: &str) -> io::Result<Opened> {
    let mut root = settings.root.as_path();
    let mut mapped = Cow::Borrowed(path);
    if let Some(alias) = &settings.alias {
        let Some(suffix) = path.strip_prefix(prefix) else {
            return Ok(Opened::Forbidden);
        };
        if alias.is_file() {
            if !suffix.is_empty() {
                return Ok(Opened::NotFound);
            }
            root = alias.parent().unwrap_or(Path::new("."));
            mapped = Cow::Owned(format!("/{}", alias.file_name().unwrap().to_string_lossy()));
        } else {
            root = alias;
            mapped = Cow::Owned(format!("/{}", suffix.trim_start_matches('/')));
        }
    }
    if settings.try_files.is_empty() {
        return open_at(root, settings, &mapped);
    }
    for (i, candidate) in settings.try_files.iter().enumerate() {
        let final_candidate = i + 1 == settings.try_files.len();
        if final_candidate && let Some(code) = candidate.strip_prefix('=') {
            return Ok(Opened::Status(code.parse().unwrap_or(404)));
        }
        let candidate = match candidate.as_str() {
            "$uri" => mapped.to_string(),
            "$uri/" => format!("{}/", mapped.trim_end_matches('/')),
            _ => candidate.clone(),
        };
        let candidate = super::normalize_decoded_path(&candidate)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        match open_at(root, settings, &candidate)? {
            Opened::NotFound | Opened::Directory if !final_candidate => continue,
            Opened::Forbidden if !final_candidate && candidate.ends_with('/') => continue,
            result => return Ok(result),
        }
    }
    Ok(Opened::NotFound)
}

// RESOLVE_CACHED refuses path walks that need I/O or revalidation. Unsupported
// filesystems, cache misses, directories, aliases and try_files use the existing
// blocking path. O_PATH lets us reject devices/FIFOs before opening for reading.
#[cfg(target_os = "linux")]
pub fn open_cached(settings: &Settings, path: &str) -> Option<Opened> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    const RESOLVE_CACHED: u64 = 0x20;
    const RESOLVE_BENEATH: u64 = 0x08;
    const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
    if settings.alias.is_some() || !settings.try_files.is_empty() {
        return None;
    }
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    fn open(dir: i32, path: &Path, flags: i32, resolve: u64) -> Option<File> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve,
        };
        // The kernel owns path resolution; a successful fd is immediately owned
        // by File, including all early-return and cancellation paths.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                dir,
                path.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            )
        };
        (fd >= 0).then(|| unsafe { File::from_raw_fd(fd as i32) })
    }
    let root = open(
        libc::AT_FDCWD,
        &settings.root,
        libc::O_RDONLY | libc::O_DIRECTORY,
        RESOLVE_CACHED,
    )?;
    let pinned = open(
        root.as_raw_fd(),
        Path::new(path.trim_start_matches('/')),
        libc::O_PATH,
        RESOLVE_CACHED | RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS,
    )?;
    let metadata = pinned.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    // Reopen this exact checked inode, never the user path after the type check.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))
        .ok()?;
    describe(file, metadata, path, settings).ok()
}

fn open_at(root_path: &Path, settings: &Settings, path: &str) -> io::Result<Opened> {
    let root = match Dir::open_ambient_dir(root_path, cap_std::ambient_authority()) {
        Ok(root) => root,
        Err(e) => return classify(e),
    };
    let relative = path.trim_start_matches('/');
    let relative = if relative.is_empty() { "." } else { relative };
    let file = match open_readonly(&root, Path::new(relative)) {
        Ok(file) => file,
        Err(e) => return classify(e),
    };
    let metadata = file.metadata()?;
    if metadata.is_dir() {
        if !path.ends_with('/') {
            return Ok(Opened::Directory);
        }
        for index in &settings.index {
            let name = Path::new(relative).join(index);
            match open_readonly(&root, &name) {
                Ok(file) => {
                    let metadata = file.metadata()?;
                    if metadata.is_file() {
                        return describe(file, metadata, index, settings);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return classify(e),
            }
        }
        return Ok(Opened::Forbidden);
    }
    if !metadata.is_file() {
        return Ok(Opened::Forbidden);
    }
    describe(file, metadata, relative, settings)
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
fn describe(file: File, metadata: Metadata, name: &str, settings: &Settings) -> io::Result<Opened> {
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

pub(super) struct Selection {
    pub response: pingora::http::ResponseHeader,
    pub start: u64,
    pub length: u64,
}
pub(super) fn select(
    file: &StaticFile,
    headers: &crate::script::RequestHeaders,
) -> pingora::Result<Selection> {
    use pingora::http::ResponseHeader;
    let not_modified = if let Some(tag) = headers.get("if-none-match") {
        tag.split(',')
            .any(|s| s.trim() == "*" || s.trim().trim_start_matches("W/") == file.etag)
    } else {
        headers
            .get("if-modified-since")
            .and_then(|s| httpdate::parse_http_date(s).ok())
            .is_some_and(|date| {
                file.modified
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    <= date
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
            })
    };
    let mut status = if not_modified { 304 } else { 200 };
    let mut start = 0;
    let mut length = file.length;
    let mut content_range = None;
    let if_range = headers.get("if-range").is_none_or(|s| {
        s == file.etag
            || httpdate::parse_http_date(s).ok().is_some_and(|d| {
                file.modified
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    == d.duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
            })
    });
    if !not_modified
        && if_range
        && let Some(range) = headers.get("range")
    {
        match self::range(range, file.length) {
            Ok(Some((a, b))) => {
                status = 206;
                start = a;
                length = b - a + 1;
                content_range = Some(format!("bytes {a}-{b}/{}", file.length));
            }
            Ok(None) => {}
            Err(()) => {
                status = 416;
                length = 0;
                content_range = Some(format!("bytes */{}", file.length));
            }
        }
    }
    let mut response = ResponseHeader::build(status, None)?;
    response.insert_header("Content-Type", file.mime.clone())?;
    response.insert_header("ETag", file.etag.clone())?;
    response.insert_header("Last-Modified", httpdate::fmt_http_date(file.modified))?;
    response.insert_header("Accept-Ranges", "bytes")?;
    if status != 304 {
        response.insert_header("Content-Length", length.to_string())?;
    }
    if let Some(range) = content_range {
        response.insert_header("Content-Range", range)?;
    }
    Ok(Selection {
        response,
        start,
        length,
    })
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
            open_route(&settings, "/escape", "/"),
            Ok(Opened::Forbidden) | Err(_)
        ));
        assert_eq!(range("bytes=-5", 3), Ok(Some((0, 2))));
        assert_eq!(range("bytes=3-", 3), Err(()));
        Ok(())
    }
}
