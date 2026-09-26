use super::{
    artifact::Candidate,
    config::{self, Config},
};
use anyhow::{Context, Result, ensure};
use std::{io::Write, path::Path};

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
pub fn save(directory: &Path, candidate: &Candidate) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    let directory = std::fs::canonicalize(directory)?;
    let object = directory.join(format!("{}.o", candidate.object_digest));
    if !object.exists() {
        atomic_write(&object, &candidate.object)?;
    }
    let mut config = candidate.config.clone();
    config.policy = Some(object);
    config.policy_sha256 = Some(candidate.object_digest.clone());
    atomic_write(
        &directory.join(format!("{}.json", candidate.digest)),
        &serde_json::to_vec_pretty(&config)?,
    )?;
    // History is explicitly bounded; retain the 20 most recently published bundles.
    let mut entries = std::fs::read_dir(&directory)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            valid_revision(&e.file_name().to_string_lossy().replace(".json", ""))
                && e.path().extension().is_some_and(|e| e == "json")
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
    for entry in entries.iter().take(entries.len().saturating_sub(20)) {
        std::fs::remove_file(entry.path())?;
    }
    let mut used = std::collections::BTreeSet::new();
    for entry in entries.iter().skip(entries.len().saturating_sub(20)) {
        if let Ok(config) = Config::read(Some(&entry.path()))
            && let Some(policy) = config.policy
        {
            used.insert(policy);
        }
    }
    for entry in std::fs::read_dir(&directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|s| s == "o")
            && path
                .file_stem()
                .is_some_and(|s| valid_revision(&s.to_string_lossy()))
            && !used.contains(&path)
        {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}
fn valid_revision(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())
}
pub fn rollback(directory: &Path, revision: &str, output: &Path) -> Result<()> {
    ensure!(
        valid_revision(revision),
        "revision must be the 64-character digest from /status"
    );
    let path = directory.join(format!("{revision}.json"));
    let config = Config::read(Some(&path))?;
    let policy = config
        .policy
        .as_ref()
        .context("history lacks compiled object")?;
    let object = config::read_bounded(policy, 4 * 1024 * 1024)?;
    ensure!(
        Some(config::digest(&object)) == config.policy_sha256,
        "history object digest mismatch"
    );
    super::artifact::metadata(&object)?;
    atomic_write(output, &serde_json::to_vec_pretty(&config)?)?;
    println!("rollback configuration published; wait for /status applied revision and readyz");
    Ok(())
}
