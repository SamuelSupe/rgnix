use super::{
    Mode,
    kernel::{Attachment, Loaded, syscall},
    maps::{OWNER_MAGIC, Owner},
};
use anyhow::{Context, Result, ensure};
use aya::maps::{Array, MapData};
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
};

pub struct Pins {
    pub path: PathBuf,
    pub owner: Owner,
    _lock: File,
    keep: bool,
}
impl Pins {
    pub fn open(interface: &str, mode: Mode, path: Option<PathBuf>, persist: bool) -> Result<Self> {
        let name = CString::new(interface)?;
        let ifindex = unsafe { libc::if_nametoindex(name.as_ptr()) };
        ensure!(ifindex != 0, "interface {interface} does not exist");
        let netns = std::fs::metadata("/proc/self/ns/net")?.ino();
        let owner = Owner {
            magic: OWNER_MAGIC,
            netns,
            ifindex,
            mode: match mode {
                Mode::Native => 4,
                Mode::Generic => 2,
            },
        };
        let lock_directory = Path::new("/run/rgnix-xdp");
        if !lock_directory.exists() {
            std::fs::create_dir(lock_directory)?;
        }
        let lock_metadata = std::fs::symlink_metadata(lock_directory)?;
        ensure!(
            lock_metadata.is_dir() && lock_metadata.uid() == unsafe { libc::geteuid() },
            "invalid XDP lock directory owner"
        );
        std::fs::set_permissions(lock_directory, std::fs::Permissions::from_mode(0o700))?;
        let lock_path = lock_directory.join(format!("{netns}-{ifindex}.lock"));
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(lock_path)?;
        ensure!(
            lock.metadata()?.uid() == unsafe { libc::geteuid() },
            "XDP lock owner mismatch"
        );
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "another XDP agent owns this interface"
        );
        let path =
            path.unwrap_or_else(|| PathBuf::from(format!("/sys/fs/bpf/rgnix-{netns}-{ifindex}")));
        if !path.exists() {
            std::fs::create_dir(&path)
                .context("create XDP pin directory; mount bpffs first or set --pin-dir")?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "pin directory must be a private directory owned by this UID"
        );
        let cpath = CString::new(path.as_os_str().as_bytes())?;
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::statfs(cpath.as_ptr(), &mut stat) } == 0
                && stat.f_type as u64 == 0xcafe4a11,
            "pin directory must be on bpffs"
        );
        if path.join("rgnix_owner").exists() {
            let data = MapData::from_pin(path.join("rgnix_owner"))?;
            let map: Array<_, Owner> = Array::try_from(aya::maps::Map::Array(data))?;
            let old = map.get(&0, 0)?;
            ensure!(
                old == owner,
                "pinned state owner, interface, namespace, mode or ABI mismatch"
            );
        } else {
            ensure!(
                std::fs::read_dir(&path)?.next().is_none(),
                "refusing an unowned nonempty pin directory"
            );
            create_owner(&path.join("rgnix_owner"), owner)?;
        }
        let existing = path.join("link").exists();
        ensure!(
            persist || !existing,
            "persistent attachment exists; use --persist or xdp detach explicitly"
        );
        Ok(Self {
            path,
            owner,
            _lock: lock,
            keep: existing,
        })
    }
    pub fn attach(
        &mut self,
        loaded: &mut Loaded,
        interface: &str,
        mode: Mode,
        persist: bool,
    ) -> Result<Attachment> {
        loaded.bind_owner(self.owner)?;
        if persist {
            self.pin_program(loaded)?;
        }
        let link = if self.path.join("link").exists() {
            let (link, old) = self.owned_link()?;
            link.replace_fd(old.as_raw_fd() as u32, loaded)?;
            link
        } else {
            let link = loaded.attach(interface, mode)?;
            if persist {
                pin(link.fd.as_raw_fd(), &self.path.join("link"))?;
            }
            link
        };
        self.keep = persist;
        if persist {
            self.prune_programs(loaded)?;
        }
        Ok(link)
    }
    pub fn pin_program(&self, loaded: &Loaded) -> Result<()> {
        let id = program_info(loaded.fd()? as i32)?.id;
        let path = self.path.join(format!("program-{id}"));
        if !path.exists() {
            pin(loaded.fd()? as i32, &path)?;
        }
        Ok(())
    }
    pub fn prune_programs(&self, loaded: &Loaded) -> Result<()> {
        let id = program_info(loaded.fd()? as i32)?.id;
        for entry in std::fs::read_dir(&self.path)? {
            let entry = entry?;
            if let Some(other) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.strip_prefix("program-"))
                .and_then(|n| n.parse::<u32>().ok())
                && other != id
            {
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
    fn owned_link(&self) -> Result<(Attachment, OwnedFd)> {
        let link = Attachment {
            fd: get(&self.path.join("link"))?,
        };
        let (ifindex, program_id) = link.identity()?;
        ensure!(
            ifindex == self.owner.ifindex,
            "pinned link belongs to a different interface"
        );
        let owner_id = MapData::from_pin(self.path.join("rgnix_owner"))?
            .info()?
            .id();
        let fd = get(&self.path.join(format!("program-{program_id}")))?;
        let program = program_info(fd.as_raw_fd())?;
        ensure!(
            program.id == program_id
                && program.name[..9] == *b"rgnix_xdp"
                && program.maps.contains(&owner_id),
            "pinned link is not owned by this rgnix state"
        );
        Ok((link, fd))
    }
    pub fn detach(mut self) -> Result<()> {
        let (link, _program) = self.owned_link()?;
        std::fs::remove_file(self.path.join("link"))?;
        drop(link);
        for entry in std::fs::read_dir(&self.path)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .and_then(|n| n.strip_prefix("program-"))
                .is_some_and(|n| n.parse::<u32>().is_ok())
            {
                std::fs::remove_file(entry.path())?;
            }
        }
        self.keep = false;
        Ok(())
    }
}
impl Drop for Pins {
    fn drop(&mut self) {
        if !self.keep {
            if let Ok(entries) = std::fs::read_dir(&self.path) {
                for entry in entries.flatten() {
                    if entry
                        .file_name()
                        .to_str()
                        .and_then(|name| name.strip_prefix("program-"))
                        .is_some_and(|id| id.parse::<u32>().is_ok())
                    {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
            for name in ["rgnix_owner", "rgnix_rates", "rgnix_keys"] {
                let _ = std::fs::remove_file(self.path.join(name));
            }
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}
fn object_path(command: u32, fd: u32, path: &Path) -> Result<libc::c_long> {
    #[repr(C)]
    struct Attr {
        pathname: u64,
        fd: u32,
        flags: u32,
    }
    let path = CString::new(path.as_os_str().as_bytes())?;
    syscall(
        command,
        &mut Attr {
            pathname: path.as_ptr() as u64,
            fd,
            flags: 0,
        },
    )
}
fn get(path: &Path) -> Result<OwnedFd> {
    let fd = object_path(7, 0, path)?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}
fn pin(fd: i32, path: &Path) -> Result<()> {
    object_path(6, fd as u32, path)?;
    Ok(())
}

struct ProgramIdentity {
    id: u32,
    name: [u8; 16],
    maps: Vec<u32>,
}
fn program_info(fd: i32) -> Result<ProgramIdentity> {
    #[repr(C)]
    #[derive(Default)]
    struct ProgramInfo {
        kind: u32,
        id: u32,
        tag: [u8; 8],
        jit_len: u32,
        translated_len: u32,
        jit: u64,
        translated: u64,
        loaded_at: u64,
        uid: u32,
        map_count: u32,
        maps: u64,
        name: [u8; 16],
        ifindex: u32,
        flags: u32,
    }
    #[repr(C)]
    struct Info {
        fd: u32,
        length: u32,
        info: u64,
    }
    let mut maps = [0u32; 64];
    let mut info = ProgramInfo {
        map_count: maps.len() as u32,
        maps: maps.as_mut_ptr() as u64,
        ..Default::default()
    };
    syscall(
        15,
        &mut Info {
            fd: fd as u32,
            length: std::mem::size_of::<ProgramInfo>() as u32,
            info: &mut info as *mut ProgramInfo as u64,
        },
    )?;
    ensure!(
        info.map_count <= 64 && info.kind == 6,
        "invalid owned XDP program"
    );
    Ok(ProgramIdentity {
        id: info.id,
        name: info.name,
        maps: maps[..info.map_count as usize].to_vec(),
    })
}

fn create_owner(path: &Path, owner: Owner) -> Result<()> {
    // Publish identity before loading any other pinned map. A crash halfway through
    // loading can then be recovered without guessing ownership of an orphan map.
    #[repr(C)]
    struct Create {
        kind: u32,
        key_size: u32,
        value_size: u32,
        max_entries: u32,
        flags: u32,
        inner: u32,
        numa: u32,
        name: [u8; 16],
        ifindex: u32,
    }
    let mut name = [0; 16];
    name[..11].copy_from_slice(b"rgnix_owner");
    let fd = syscall(
        0,
        &mut Create {
            kind: 2,
            key_size: 4,
            value_size: std::mem::size_of::<Owner>() as u32,
            max_entries: 1,
            flags: 0,
            inner: 0,
            numa: 0,
            name,
            ifindex: 0,
        },
    )?;
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let zero = 0u32;
    #[repr(C)]
    struct Update {
        fd: u32,
        pad: u32,
        key: u64,
        value: u64,
        flags: u64,
    }
    syscall(
        2,
        &mut Update {
            fd: fd.as_raw_fd() as u32,
            pad: 0,
            key: &zero as *const u32 as u64,
            value: &owner as *const Owner as u64,
            flags: 0,
        },
    )?;
    pin(fd.as_raw_fd(), path)
}
