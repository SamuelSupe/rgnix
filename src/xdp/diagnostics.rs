use super::{artifact::Inputs, config, kernel::Loaded};
use anyhow::{Context, Result, ensure};
use std::{net::SocketAddr, path::Path, time::Duration};

pub fn doctor(interface: &str) -> Result<()> {
    ensure!(
        !interface.is_empty() && interface.len() < libc::IFNAMSIZ,
        "invalid interface"
    );
    let output = std::process::Command::new("ip")
        .args(["-j", "-d", "link", "show", "dev", interface])
        .output()
        .context("iproute2 is required for interface diagnostics")?;
    ensure!(
        output.status.success(),
        "interface inspection failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let link: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let status = std::fs::read_to_string("/proc/self/status")?;
    let capabilities: Vec<_> = status
        .lines()
        .filter(|line| {
            line.starts_with("Cap") || line.starts_with("NoNewPrivs") || line.starts_with("Seccomp")
        })
        .collect();
    let mounts = std::fs::read_to_string("/proc/mounts")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "kernel": std::fs::read_to_string("/proc/sys/kernel/osrelease")?.trim(), "architecture": std::env::consts::ARCH,
            "interface": link, "capabilities": capabilities, "bpffs_mounted": mounts.lines().any(|l| l.split_whitespace().nth(2) == Some("bpf")),
            "kernel_btf": Path::new("/sys/kernel/btf/vmlinux").exists(),
            "requirements": ["CAP_BPF, CAP_NET_ADMIN, CAP_PERFMON", "bpffs writable private pin directory", "native or generic selected explicitly", "existing XDP attachment must not be overwritten"],
            "cni": "rgnix run refuses existing attachments. For a libxdp-compatible CNI, compile --dispatcher produces an immutable BTF component for xdp-loader; it must never replace an incompatible legacy program.",
            "validation": "Read-only preflight; native NIC support and kernel verifier acceptance must be tested on the target interface/kernel."
        }))?
    );
    Ok(())
}
pub fn status(admin: SocketAddr, ready: bool, health: bool) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()?;
    if ready || health {
        client
            .get(format!(
                "http://{admin}/{}",
                if ready { "readyz" } else { "healthz" }
            ))
            .send()?
            .error_for_status()?;
    }
    if health {
        println!("ok");
        return Ok(());
    }
    let response = client
        .get(format!("http://{admin}/status"))
        .send()?
        .error_for_status()?;
    println!("{}", response.text()?);
    Ok(())
}
pub fn replay(source: &Path, pcap: &Path, config: Option<&Path>, clang: &Path) -> Result<()> {
    let loaded = Loaded::configured(Inputs::read(source, config)?.prepare(clang, None)?, None)?;
    let bytes = config::read_bounded(pcap, 64 * 1024 * 1024)?;
    ensure!(bytes.len() >= 24, "truncated PCAP header");
    let little = match &bytes[..4] {
        [0xd4, 0xc3, 0xb2, 0xa1] | [0x4d, 0x3c, 0xb2, 0xa1] => true,
        [0xa1, 0xb2, 0xc3, 0xd4] | [0xa1, 0xb2, 0x3c, 0x4d] => false,
        _ => anyhow::bail!("expected classic PCAP (pcapng is not supported)"),
    };
    let word = |at: usize| {
        let a: [u8; 4] = bytes[at..at + 4].try_into().unwrap();
        if little {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    };
    ensure!(word(20) == 1, "PCAP link type must be Ethernet");
    let mut offset = 24;
    let mut index = 0;
    while offset < bytes.len() {
        ensure!(
            index < 10000 && offset + 16 <= bytes.len(),
            "PCAP exceeds 10000 frames or has a truncated record"
        );
        let size = word(offset + 8) as usize;
        ensure!(
            size <= 65536 && offset + 16 + size <= bytes.len(),
            "invalid PCAP frame length"
        );
        ensure!(
            size == word(offset + 12) as usize,
            "PCAP contains snaplen-truncated frame {index}; replay needs the complete Ethernet packet"
        );
        let before = loaded.rule_stats()?;
        let result = loaded.test(&bytes[offset + 16..offset + 16 + size], 1)?;
        let after = loaded.rule_stats()?;
        let hits: Vec<_> = before
            .iter()
            .zip(&after)
            .filter_map(|(old, new)| {
                let action = if new.drop.packets > old.drop.packets {
                    "drop"
                } else if new.would_drop.packets > old.would_drop.packets {
                    "would_drop"
                } else if new.pass.packets > old.pass.packets {
                    "pass"
                } else {
                    return None;
                };
                Some(serde_json::json!({"rule": new.rule, "action": action}))
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({"packet": index, "action": result["action"], "duration_ns": result["duration_ns"], "hits": hits})
        );
        index += 1;
        offset += 16 + size;
    }
    Ok(())
}
