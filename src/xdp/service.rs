use super::{
    Mode,
    artifact::{Candidate, Inputs},
    kernel::Loaded,
    lifecycle::Pins,
    maps::{RuleCount, RuleStats},
};
use anyhow::{Context, Result};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::net::TcpListener;

pub struct Options {
    pub source: PathBuf,
    pub interface: String,
    pub mode: Mode,
    pub admin: SocketAddr,
    pub config: Option<PathBuf>,
    pub watch_interval: u64,
    pub pin_dir: Option<PathBuf>,
    pub persist: bool,
    pub history_dir: Option<PathBuf>,
    pub otlp: crate::otlp::Options,
    pub clang: PathBuf,
}
pub(super) struct State {
    pub loaded: Loaded,
    pub previous: [u64; 16],
    pub previous_rules: BTreeMap<String, [RuleCount; 3]>,
    pub generation: u64,
    pub reload_failures: u64,
    pub desired: String,
    pub last_error: Option<String>,
    pub history_error: Option<String>,
    pub applied_at: u64,
    pub interface: String,
    pub mode: Mode,
    pub persistent: bool,
    pub rate_entries: usize,
    pub registry: prometheus::Registry,
}
impl State {
    pub fn rules(&self) -> Result<Vec<RuleStats>> {
        let mut rows = self.loaded.rule_stats()?;
        for row in &mut rows {
            if let Some(old) = self.previous_rules.get(&row.rule) {
                for (value, old) in [&mut row.pass, &mut row.drop, &mut row.would_drop]
                    .into_iter()
                    .zip(old)
                {
                    value.packets = value.packets.saturating_add(old.packets);
                    value.bytes = value.bytes.saturating_add(old.bytes);
                }
            }
        }
        Ok(rows)
    }
    pub fn failed(&mut self, error: anyhow::Error) {
        self.reload_failures += 1;
        self.last_error = Some(format!("{error:#}"));
        log::error!("XDP update rejected; active policy retained: {error:#}");
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn run(options: Options) -> Result<()> {
    let initial =
        Inputs::read(&options.source, options.config.as_deref())?.prepare(&options.clang, None)?;
    anyhow::ensure!(
        initial.metadata.dispatcher_config.is_none(),
        "dispatcher artifacts must be attached with libxdp/xdp-loader; compile without --dispatcher for rgnix run"
    );
    let mut pins = Pins::open(
        &options.interface,
        options.mode,
        options.pin_dir.clone(),
        options.persist,
    )?;
    let loaded = Loaded::configured(initial, Some(&pins.path))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let listener = TcpListener::bind(options.admin).await.context("bind XDP management listener")?;
        let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let registry = prometheus::Registry::new();
        let exporter = crate::otlp::Exporter::start_network(options.otlp.clone(), &registry)?;
        let mut state = State { desired: loaded.candidate.input_digest.clone(), loaded, previous: [0; 16], previous_rules: BTreeMap::new(), generation: 1, reload_failures: 0,
            last_error: None, history_error: None, applied_at: now(), interface: options.interface.clone(), mode: options.mode, persistent: options.persist, rate_entries: 0, registry };
        let attachment = pins.attach(&mut state.loaded, &options.interface, options.mode, options.persist)?;
        save_history(&mut state, &options);
        let state = Arc::new(Mutex::new(state));
        let admin_state = state.clone();
        let mut management = tokio::spawn(async move { super::management::serve(listener, admin_state).await });
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut ticks = 0u64;
        let mut attempted = state.lock().unwrap().loaded.candidate.input_digest.clone();
        log::info!("XDP attached interface={} mode={:?} admin={} persistent={}", options.interface, options.mode, options.admin, options.persist);
        'agent: loop {
            let forced = tokio::select! {
                _ = term.recv() => break,
                _ = interrupt.recv() => break,
                result = &mut management => { result??; anyhow::bail!("XDP management stopped"); }
                _ = hup.recv() => true,
                _ = tick.tick() => {
                    attachment.check()?;
                    ticks += 1;
                    let mut active = state.lock().unwrap();
                    super::events::export(&mut active.loaded, exporter.as_ref())?;
                    if ticks.is_multiple_of(150) { active.rate_entries = active.loaded.rate_entries()?; }
                    if options.watch_interval == 0 || !ticks.is_multiple_of(5 * options.watch_interval) { continue; }
                    false
                }
            };
            let inputs = match Inputs::read(&options.source, options.config.as_deref()) {
                Ok(inputs) => inputs,
                Err(error) => {
                    let key = format!("invalid:{error:#}");
                    if forced || attempted != key {
                        attempted = key;
                        let mut active = state.lock().unwrap(); active.desired = "invalid".into(); active.failed(error);
                    }
                    continue;
                }
            };
            if !forced && inputs.digest == attempted { continue; }
            attempted = inputs.digest.clone();
            let old: Candidate = {
                let mut active = state.lock().unwrap(); active.desired = inputs.digest.clone(); active.loaded.candidate.clone()
            };
            let clang = options.clang.clone(); let path = pins.path.clone();
            let mut worker = tokio::task::spawn_blocking(move || -> Result<Loaded> {
                let candidate = inputs.prepare(&clang, Some(&old))?;
                anyhow::ensure!(candidate.metadata.dispatcher_config.is_none(), "cannot publish a dispatcher artifact to an exclusive agent");
                Loaded::configured(candidate, Some(&path))
            });
            let result = loop {
                tokio::select! {
                    _ = term.recv() => break 'agent,
                    _ = interrupt.recv() => break 'agent,
                    _ = tick.tick() => { attachment.check()?; super::events::export(&mut state.lock().unwrap().loaded, exporter.as_ref())?; }
                    result = &mut management => { result??; anyhow::bail!("XDP management stopped"); }
                    result = &mut worker => break result.unwrap_or_else(|error| Err(error.into())),
                }
            };
            let mut active = state.lock().unwrap();
            let result = result.and_then(|mut next| {
                if next.candidate.digest == active.loaded.candidate.digest {
                    active.loaded.candidate = next.candidate;
                    active.last_error = None; return Ok(());
                }
                next.bind_owner(pins.owner)?;
                let previous = active.loaded.all_stats()?;
                let rules = active.rules()?;
                if options.persist { pins.pin_program(&next)?; }
                attachment.replace(&active.loaded, &next)?;
                for (i, count) in previous.into_iter().enumerate() { active.previous[i] = active.previous[i].saturating_add(count); }
                active.previous_rules = rules.into_iter().filter(|row| next.candidate.metadata.rules.iter().any(|r| r.name == row.rule) || ["default", "scope_bypass", "malformed", "unsupported", "fragment", "global_ceiling"].contains(&row.rule.as_str())).map(|row| (row.rule, [row.pass, row.drop, row.would_drop])).collect();
                active.loaded = next;
                if options.persist && let Err(error) = pins.prune_programs(&active.loaded) { log::warn!("XDP program pin cleanup: {error:#}"); }
                active.generation += 1; active.applied_at = now(); active.last_error = None;
                if let Err(error) = active.loaded.prune_rates() { log::warn!("XDP old limiter state cleanup: {error:#}"); }
                save_history(&mut active, &options);
                log::info!("XDP policy published generation={} revision={}", active.generation, active.loaded.candidate.digest);
                Ok(())
            });
            if let Err(error) = result { active.failed(error); }
        }
        management.abort();
        drop(attachment);
        log::info!("XDP agent stopped; persistent={}", options.persist);
        Ok(())
    })
}
fn save_history(state: &mut State, options: &Options) {
    if let Some(path) = &options.history_dir {
        state.history_error = super::history::save(path, &state.loaded.candidate)
            .err()
            .map(|e| format!("{e:#}"));
        if let Some(error) = &state.history_error {
            log::error!("XDP history persistence failed: {error}");
        }
    }
}
