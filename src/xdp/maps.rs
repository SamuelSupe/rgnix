use super::{
    config::{self, Action},
    kernel::Loaded,
};
use anyhow::{Context, Result, ensure};
use aya::{
    Pod,
    maps::{
        Array, HashMap, PerCpuArray,
        lpm_trie::{Key, LpmTrie},
    },
};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    time::{SystemTime, UNIX_EPOCH},
};

pub const OWNER_MAGIC: u64 = 0x52474e4958584432;
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq)]
pub struct Owner {
    pub magic: u64,
    pub netns: u64,
    pub ifindex: u32,
    pub mode: u32,
}
// These ABI structs contain only initialized integers/arrays, with explicit padding.
unsafe impl Pod for Owner {}
#[repr(C)]
#[derive(Clone, Copy)]
struct Settings {
    observe: u32,
    malformed: u32,
    unsupported: u32,
    fragments: u32,
    destinations: u32,
    port_count: u32,
    protocol_count: u32,
    sample_every: u32,
    event_rate: u32,
    ceiling_rate: u32,
    ceiling_burst: u32,
    pad: u32,
    ceiling_id: u64,
    ports: [u16; 32],
    protocols: [u8; 16],
}
unsafe impl Pod for Settings {}
#[repr(C)]
#[derive(Clone, Copy)]
struct SetValue {
    expires: u64,
    prefix: u32,
    pad: u32,
}
unsafe impl Pod for SetValue {}
#[repr(C)]
#[derive(Clone, Copy, Default, Serialize)]
pub struct RuleCount {
    pub packets: u64,
    pub bytes: u64,
}
unsafe impl Pod for RuleCount {}
#[derive(Serialize)]
pub struct RuleStats {
    pub rule: String,
    pub pass: RuleCount,
    pub drop: RuleCount,
    pub would_drop: RuleCount,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Event {
    pub timestamp: u64,
    pub length: u32,
    pub rule: u32,
    pub reason: u32,
    pub observed: u32,
    pub src: [u8; 16],
    pub dst: [u8; 16],
    pub src_port: u16,
    pub dst_port: u16,
    pub version: u8,
    pub protocol: u8,
    pub pad: u16,
}
unsafe impl Pod for Event {}
pub fn monotonic_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // CLOCK_MONOTONIC shares bpf_ktime_get_ns's time base.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}
fn verdict(action: Action) -> u32 {
    match action {
        Action::Pass => 2,
        Action::Drop => 1,
        Action::Policy => 0,
    }
}
impl Loaded {
    pub(super) fn configure(&mut self) -> Result<()> {
        let config = &self.candidate.config;
        let mut settings = Settings {
            observe: u32::from(config.observe),
            malformed: verdict(config.malformed),
            unsupported: verdict(config.unsupported),
            fragments: verdict(config.fragments),
            destinations: config.scope.destinations.len() as u32,
            port_count: config.scope.ports.len() as u32,
            protocol_count: config.scope.protocols.len() as u32,
            sample_every: config.event_sample_every,
            event_rate: config.event_max_per_second,
            ceiling_rate: config.ceiling_pps,
            ceiling_burst: config.ceiling_burst,
            pad: 0,
            ceiling_id: config::id(
                format!("ceiling:{}:{}", config.ceiling_pps, config.ceiling_burst).as_bytes(),
            ),
            ports: [0; 32],
            protocols: [0; 16],
        };
        settings.ports[..config.scope.ports.len()].copy_from_slice(&config.scope.ports);
        settings.protocols[..config.scope.protocols.len()].copy_from_slice(&config.scope.protocols);
        let mut map: Array<_, Settings> = Array::try_from(
            self.bpf
                .map_mut("rgnix_config")
                .context("missing settings map")?,
        )?;
        map.set(0, settings, 0)?;
        let mut owner: Array<_, Owner> = Array::try_from(
            self.bpf
                .map_mut("rgnix_owner")
                .context("missing owner map")?,
        )?;
        let identity = owner.get(&0, 0)?;
        ensure!(
            identity.magic == 0 || identity.magic == OWNER_MAGIC,
            "pinned owner map has incompatible ABI"
        );
        if identity.magic == 0 {
            owner.set(
                0,
                Owner {
                    magic: OWNER_MAGIC,
                    ..Default::default()
                },
                0,
            )?;
        }
        let mut sets: LpmTrie<_, [u8; 24], SetValue> =
            LpmTrie::try_from(self.bpf.map_mut("rgnix_sets").context("missing sets map")?)?;
        let wall = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let mono = monotonic_ns();
        let mut insert = |set: u32, cidr: ipnet::IpNet, expires_at: u64| -> Result<()> {
            let deadline = u128::from(expires_at) * 1_000_000_000;
            if expires_at != 0 && deadline <= wall {
                return Ok(());
            }
            let expires = if expires_at == 0 {
                0
            } else {
                mono.saturating_add((deadline - wall).min(u128::from(u64::MAX)) as u64)
            };
            let mut bytes = [0; 24];
            bytes[..4].copy_from_slice(&set.to_ne_bytes());
            let bits = cidr.prefix_len() as u32;
            match cidr.trunc() {
                ipnet::IpNet::V4(net) => {
                    bytes[4..8].copy_from_slice(&4u32.to_ne_bytes());
                    bytes[8..12].copy_from_slice(&net.network().octets());
                }
                ipnet::IpNet::V6(net) => {
                    bytes[4..8].copy_from_slice(&6u32.to_ne_bytes());
                    bytes[8..].copy_from_slice(&net.network().octets());
                }
            }
            sets.insert(
                &Key::new(64 + bits, bytes),
                SetValue {
                    expires,
                    prefix: 64 + bits,
                    pad: 0,
                },
                0,
            )?;
            Ok(())
        };
        for cidr in &config.scope.destinations {
            insert(u32::MAX, *cidr, 0)?;
        }
        for (slot, name) in self.candidate.metadata.sets.iter().enumerate() {
            let entries = config
                .sets
                .get(name)
                .with_context(|| format!("missing address set {name}"))?;
            for entry in entries {
                insert(slot as u32, entry.cidr, entry.expires_at)?;
            }
        }
        Ok(())
    }
    pub fn bind_owner(&mut self, identity: Owner) -> Result<()> {
        let mut owner: Array<_, Owner> = Array::try_from(self.bpf.map_mut("rgnix_owner").unwrap())?;
        let previous = owner.get(&0, 0)?;
        ensure!(
            previous.ifindex == 0 || previous == identity,
            "pinned state belongs to another interface, network namespace or mode"
        );
        owner.set(0, identity, 0)?;
        Ok(())
    }
    pub fn all_stats(&self) -> Result<[u64; 16]> {
        let map: PerCpuArray<_, u64> = PerCpuArray::try_from(self.bpf.map("rgnix_stats").unwrap())?;
        let mut result = [0; 16];
        for (i, value) in result.iter_mut().enumerate() {
            *value = map.get(&(i as u32), 0)?.iter().copied().sum();
        }
        Ok(result)
    }
    pub fn rule_name(&self, slot: u32) -> String {
        const SYSTEM: [&str; 8] = [
            "default",
            "scope_bypass",
            "malformed",
            "unsupported",
            "fragment",
            "global_ceiling",
            "reserved6",
            "reserved7",
        ];
        if slot < 8 {
            SYSTEM[slot as usize].into()
        } else {
            self.candidate
                .metadata
                .rules
                .get(slot as usize - 8)
                .map(|r| r.name.clone())
                .unwrap_or_else(|| "unknown".into())
        }
    }
    pub fn rule_stats(&self) -> Result<Vec<RuleStats>> {
        let map: PerCpuArray<_, RuleCount> = PerCpuArray::try_from(
            self.bpf
                .map("rgnix_rules")
                .context("missing rule counters")?,
        )?;
        let mut rows = vec![];
        for slot in 0..8 + self.candidate.metadata.rules.len() as u32 {
            let mut counts = [RuleCount::default(); 3];
            for (action, count) in counts.iter_mut().enumerate() {
                for cpu in map.get(&(slot * 3 + action as u32), 0)?.iter() {
                    count.packets += cpu.packets;
                    count.bytes += cpu.bytes;
                }
            }
            rows.push(RuleStats {
                rule: self.rule_name(slot),
                pass: counts[0],
                drop: counts[1],
                would_drop: counts[2],
            });
        }
        Ok(rows)
    }
    pub fn rate_entries(&self) -> Result<usize> {
        let map: HashMap<_, [u8; 32], u64> =
            HashMap::try_from(self.bpf.map("rgnix_keys").context("missing keyed rates")?)?;
        // Bounded even if concurrent LRU churn causes the kernel's iterator to revisit keys.
        Ok(map
            .keys()
            .take(config::RATE_CAPACITY as usize)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .len())
    }
    pub fn prune_rates(&mut self) -> Result<()> {
        let config = &self.candidate.config;
        let mut ids: BTreeSet<u64> = self.candidate.metadata.rate_ids.iter().copied().collect();
        ids.insert(0);
        ids.insert(config::id(
            format!("ceiling:{}:{}", config.ceiling_pps, config.ceiling_burst).as_bytes(),
        ));
        let mut map: HashMap<_, u64, u64> =
            HashMap::try_from(self.bpf.map_mut("rgnix_rates").unwrap())?;
        let keys: Vec<_> = map
            .keys()
            .take(256)
            .collect::<std::result::Result<_, _>>()?;
        for key in keys {
            if !ids.contains(&key) {
                map.remove(&key)?;
            }
        }
        Ok(())
    }
    pub fn events(&mut self) -> Result<Vec<Event>> {
        let mut events = vec![];
        for _ in 0..256 {
            let Some(bytes) = self.event_ring.next() else {
                break;
            };
            ensure!(
                bytes.len() == std::mem::size_of::<Event>(),
                "invalid event ABI"
            );
            // Ringbuf records may be unaligned; all event bit patterns are valid integers.
            events.push(unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<Event>()) });
        }
        Ok(events)
    }
}
