use super::{CompiledScript, Compiler, PendingCompilation};
use anyhow::{Result, bail, ensure};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Condvar, Mutex, Weak},
    time::{Duration, Instant},
};

pub(super) struct Queue {
    state: Mutex<State>,
    changed: Condvar,
}
#[derive(Default)]
struct State {
    jobs: VecDeque<Job>,
    pending: HashMap<[u8; 32], String>,
    budgets: HashMap<String, (Instant, u32)>,
}
struct Job {
    key: [u8; 32],
    source: Vec<u8>,
}
impl Queue {
    pub fn start(compiler: Weak<Compiler>) -> Arc<Self> {
        let queue = Arc::new(Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let worker = queue.clone();
        std::thread::Builder::new()
            .name("rgnix-compiler".into())
            .spawn(move || {
                loop {
                    let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
                    while state.jobs.is_empty() {
                        if compiler.strong_count() == 0 {
                            return;
                        }
                        state = worker
                            .changed
                            .wait_timeout(state, Duration::from_secs(1))
                            .unwrap_or_else(|e| e.into_inner())
                            .0;
                    }
                    let job = state.jobs.pop_front().unwrap();
                    drop(state);
                    let Some(compiler) = compiler.upgrade() else {
                        return;
                    };
                    if let Ok(script) = compiler.from_bytes(&job.source, false) {
                        let mut warm = compiler.warm.lock().unwrap_or_else(|e| e.into_inner());
                        warm.retain(|(at, _)| at.elapsed() < Duration::from_secs(30));
                        while warm.len() >= 64 {
                            warm.pop_front();
                        }
                        warm.push_back((Instant::now(), script));
                    }
                    worker
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .pending
                        .remove(&job.key);
                    worker.changed.notify_all();
                    compiler
                        .generation
                        .fetch_add(1, std::sync::atomic::Ordering::Release);
                    compiler.changed.notify_one();
                }
            })
            .expect("start compiler worker");
        queue
    }
    pub fn submit(
        &self,
        key: [u8; 32],
        namespace: &str,
        source: &[u8],
        per_minute: u32,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.pending.contains_key(&key) {
            return Ok(());
        }
        ensure!(
            state.pending.len() < 128,
            PendingCompilation("compiler queue is full; update will be retried")
        );
        ensure!(
            state
                .pending
                .values()
                .filter(|ns| ns.as_str() == namespace)
                .count()
                < 2,
            PendingCompilation("namespace compiler queue is full; update will be retried")
        );
        state
            .budgets
            .retain(|_, (at, _)| at.elapsed() < Duration::from_secs(60));
        ensure!(
            state.budgets.len() < 4096 || state.budgets.contains_key(namespace),
            PendingCompilation("compiler tenant budget capacity reached")
        );
        let budget = state
            .budgets
            .entry(namespace.into())
            .or_insert((Instant::now(), 0));
        ensure!(
            budget.1 < per_minute,
            PendingCompilation("namespace compilation rate exceeded; update will be retried")
        );
        budget.1 += 1;
        state.pending.insert(key, namespace.into());
        state.jobs.push_back(Job {
            key,
            source: source.to_vec(),
        });
        self.changed.notify_all();
        Ok(())
    }
    pub fn wait(&self, compiler: &Compiler, key: &[u8; 32]) -> Result<Arc<CompiledScript>> {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(result) = compiler.cached(key)? {
                return result;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                bail!(PendingCompilation("compilation pending; retry validation"));
            };
            state = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}
