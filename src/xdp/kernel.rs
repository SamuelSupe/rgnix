use super::Mode;
use super::artifact::Candidate;
use anyhow::{Context, Result, ensure};
use aya::{Ebpf, EbpfLoader, maps::PerCpuArray, programs::Xdp};
use std::path::Path;
use std::{
    ffi::CString,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
};

const BPF_PROG_TEST_RUN: u32 = 10;
const BPF_OBJ_GET_INFO_BY_FD: u32 = 15;
const BPF_LINK_CREATE: u32 = 28;
const BPF_LINK_UPDATE: u32 = 29;
const BPF_XDP: u32 = 37;
const BPF_F_REPLACE: u32 = 4;
const XDP_FLAGS_SKB_MODE: u32 = 2;
const XDP_FLAGS_DRV_MODE: u32 = 4;

pub struct Loaded {
    pub(super) bpf: Ebpf,
    pub candidate: Candidate,
    pub(super) event_ring: aya::maps::RingBuf<aya::maps::MapData>,
}
pub struct Attachment {
    pub(super) fd: OwnedFd,
}

impl Loaded {
    pub fn configured(candidate: Candidate, pins: Option<&Path>) -> Result<Self> {
        ensure!(
            cfg!(target_endian = "little"),
            "XDP objects require little-endian Linux"
        );
        let mut loader = EbpfLoader::new();
        if let Some(path) = pins {
            for name in ["rgnix_owner", "rgnix_rates", "rgnix_keys"] {
                loader.map_pin_path(name, path.join(name));
            }
        }
        let mut bpf = loader
            .load(&candidate.object)
            .context("load XDP object/maps")?;
        let program: &mut Xdp = bpf
            .program_mut("rgnix_xdp")
            .context("object must contain rgnix_xdp")?
            .try_into()?;
        program.load().context("kernel rejected XDP program; check policy.rgl source lines in verifier log and xdp doctor")?;
        let event_ring = aya::maps::RingBuf::try_from(
            bpf.take_map("rgnix_events").context("missing event ring")?,
        )?;
        let mut loaded = Self {
            bpf,
            candidate,
            event_ring,
        };
        loaded.configure()?;
        loaded.stats()?;
        Ok(loaded)
    }
    pub(super) fn fd(&self) -> Result<u32> {
        Ok(self
            .bpf
            .program("rgnix_xdp")
            .context("missing XDP program")?
            .fd()?
            .as_fd()
            .as_raw_fd() as u32)
    }
    pub fn attach(&self, interface: &str, mode: Mode) -> Result<Attachment> {
        ensure!(
            !interface.is_empty() && interface.len() < libc::IFNAMSIZ,
            "invalid interface name"
        );
        let name = CString::new(interface)?;
        // CString lives through this read-only libc call.
        let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        ensure!(index != 0, "network interface {interface} does not exist");
        #[repr(C)]
        struct Create {
            prog_fd: u32,
            target_ifindex: u32,
            attach_type: u32,
            flags: u32,
        }
        let mut attr = Create {
            prog_fd: self.fd()?,
            target_ifindex: index,
            attach_type: BPF_XDP,
            flags: match mode {
                Mode::Native => XDP_FLAGS_DRV_MODE,
                Mode::Generic => XDP_FLAGS_SKB_MODE,
            },
        };
        // BPF links give exclusive ownership. No legacy netlink fallback may overwrite a CNI.
        let fd = syscall(BPF_LINK_CREATE, &mut attr).context("attach XDP link; interface must have no existing XDP program and support the selected mode")?;
        Ok(Attachment {
            fd: unsafe { OwnedFd::from_raw_fd(fd as i32) },
        })
    }
    pub fn stats(&self) -> Result<[u64; 4]> {
        let map: PerCpuArray<_, u64> = PerCpuArray::try_from(
            self.bpf
                .map("rgnix_stats")
                .context("missing rgnix_stats map")?,
        )?;
        ensure!(map.len() == 16, "unsupported XDP metrics ABI");
        let mut stats = [0; 4];
        for (index, value) in stats.iter_mut().enumerate() {
            *value = map.get(&(index as u32), 0)?.iter().copied().sum();
        }
        Ok(stats)
    }
    pub fn test(&self, frame: &[u8], repeat: u32) -> Result<serde_json::Value> {
        #[repr(C)]
        #[derive(Default)]
        struct Test {
            prog_fd: u32,
            retval: u32,
            data_size_in: u32,
            data_size_out: u32,
            data_in: u64,
            data_out: u64,
            repeat: u32,
            duration: u32,
            ctx_size_in: u32,
            ctx_size_out: u32,
            ctx_in: u64,
            ctx_out: u64,
            flags: u32,
            cpu: u32,
            batch_size: u32,
            padding: u32,
        }
        let mut attr = Test {
            prog_fd: self.fd()?,
            data_size_in: frame.len() as u32,
            data_in: frame.as_ptr() as u64,
            repeat,
            ..Default::default()
        };
        syscall(BPF_PROG_TEST_RUN, &mut attr).context("BPF_PROG_TEST_RUN failed")?;
        Ok(
            serde_json::json!({"action": match attr.retval { 1 => "drop", 2 => "pass", _ => "unexpected" }, "retval": attr.retval, "duration_ns": attr.duration, "repeat": repeat, "counters": self.stats()?, "rules": self.rule_stats()?}),
        )
    }
}
impl Attachment {
    pub fn check(&self) -> Result<()> {
        self.identity()?;
        Ok(())
    }
    pub(super) fn identity(&self) -> Result<(u32, u32)> {
        #[repr(C)]
        #[derive(Default)]
        struct LinkInfo {
            kind: u32,
            id: u32,
            prog_id: u32,
            padding: u32,
            ifindex: u32,
            tail_padding: u32,
        }
        #[repr(C)]
        struct Info {
            fd: u32,
            length: u32,
            info: u64,
        }
        let mut info = LinkInfo::default();
        let mut attr = Info {
            fd: self.fd.as_raw_fd() as u32,
            length: std::mem::size_of::<LinkInfo>() as u32,
            info: &mut info as *mut LinkInfo as u64,
        };
        syscall(BPF_OBJ_GET_INFO_BY_FD, &mut attr).context("inspect owned XDP link")?;
        ensure!(
            info.ifindex != 0,
            "XDP interface disappeared or link was detached"
        );
        ensure!(info.kind == 6, "pinned object is not an XDP link");
        Ok((info.ifindex, info.prog_id))
    }

    pub fn replace(&self, previous: &Loaded, next: &Loaded) -> Result<()> {
        self.replace_fd(previous.fd()?, next)
    }
    pub(super) fn replace_fd(&self, old_fd: u32, next: &Loaded) -> Result<()> {
        #[repr(C)]
        struct Update {
            link_fd: u32,
            new_prog_fd: u32,
            flags: u32,
            old_prog_fd: u32,
        }
        let mut attr = Update {
            link_fd: self.fd.as_raw_fd() as u32,
            new_prog_fd: next.fd()?,
            flags: BPF_F_REPLACE,
            old_prog_fd: old_fd,
        };
        // Borrow the link FD: a failed replacement must never detach the old policy.
        syscall(BPF_LINK_UPDATE, &mut attr)
            .context("atomic XDP replacement failed; old program retained")?;
        Ok(())
    }
}
pub(super) fn syscall<T>(command: u32, attr: &mut T) -> Result<libc::c_long> {
    // Only repr(C) BPF UAPI attributes with initialized padding reach this function.
    // The kernel copies the attributes synchronously and never retains this pointer.
    let result = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            command,
            attr as *mut T,
            std::mem::size_of::<T>(),
        )
    };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(result)
}
