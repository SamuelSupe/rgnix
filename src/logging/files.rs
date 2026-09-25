use super::rotation::{self, FileSink, Rotation};
use anyhow::Result;
use arc_swap::{ArcSwap, ArcSwapOption};
use prometheus::{IntCounter, IntCounterVec, IntGauge, Registry};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Access,
    Error,
}
enum Message {
    Record(PathBuf, String, Kind),
    Wake,
}

#[derive(Clone)]
struct Metrics {
    access_dropped: IntCounter,
    error_dropped: IntCounter,
    io_errors: IntCounterVec,
    rotations: IntCounter,
    reopens: IntCounter,
    pending: IntGauge,
    shutdown_pending: IntCounter,
}
impl Metrics {
    fn drop_record(&self, kind: Kind) {
        match kind {
            Kind::Access => &self.access_dropped,
            Kind::Error => &self.error_dropped,
        }
        .inc();
    }
}

pub struct FileLogs {
    sender: ArcSwapOption<SyncSender<Message>>,
    policy: Arc<ArcSwap<Rotation>>,
    reopen: Arc<AtomicBool>,
    metrics: Metrics,
    worker: Mutex<Option<(Receiver<()>, JoinHandle<()>)>>,
    signal: signal_hook::SigId,
}
impl FileLogs {
    pub fn start(registry: &Registry, access_dropped: IntCounter) -> Result<Arc<Self>> {
        let error_dropped = IntCounter::new(
            "rgnix_error_logs_dropped_total",
            "Error records dropped by queue overflow or file I/O failure",
        )?;
        let rotations = IntCounter::new(
            "rgnix_log_rotations_total",
            "Successful local log file rotations",
        )?;
        let reopens = IntCounter::new(
            "rgnix_log_reopens_total",
            "Successful local log file reopen operations",
        )?;
        let io_errors = IntCounterVec::new(
            prometheus::Opts::new("rgnix_log_io_errors_total", "Local log filesystem failures"),
            &["operation"],
        )?;
        let pending = IntGauge::new(
            "rgnix_file_logs_pending",
            "Local log records queued or being written",
        )?;
        let shutdown_pending = IntCounter::new(
            "rgnix_file_logs_shutdown_pending_total",
            "Records still pending when the file log shutdown deadline expired",
        )?;
        registry.register(Box::new(shutdown_pending.clone()))?;
        for counter in [&error_dropped, &rotations, &reopens] {
            registry.register(Box::new(counter.clone()))?;
        }
        registry.register(Box::new(io_errors.clone()))?;
        registry.register(Box::new(pending.clone()))?;
        let metrics = Metrics {
            access_dropped,
            error_dropped,
            io_errors,
            rotations,
            reopens,
            pending,
            shutdown_pending,
        };
        let worker_metrics = metrics.clone();
        let policy = Arc::new(ArcSwap::from_pointee(Rotation::default()));
        let worker_policy = policy.clone();
        let reopen = Arc::new(AtomicBool::new(false));
        // Register before listeners start so an early USR1 cannot terminate the process.
        let signal = signal_hook::flag::register(signal_hook::consts::SIGUSR1, reopen.clone())?;
        let worker_reopen = reopen.clone();
        let (sender, receiver) = mpsc::sync_channel(4096);
        let (done, finished) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("file-logs".into())
            .spawn(move || {
                run(receiver, worker_policy, worker_reopen, worker_metrics);
                let _ = done.send(());
            })?;
        Ok(Arc::new(Self {
            sender: ArcSwapOption::from(Some(Arc::new(sender))),
            policy,
            reopen,
            metrics,
            worker: Mutex::new(Some((finished, worker))),
            signal,
        }))
    }

    pub fn configure(&self, policy: Rotation) {
        self.policy.store(Arc::new(policy));
        self.reopen();
    }
    pub fn reopen(&self) {
        self.reopen.store(true, Ordering::Release);
        if let Some(sender) = self.sender.load().as_ref() {
            let _ = sender.try_send(Message::Wake);
        }
    }
    pub(super) fn write(&self, path: PathBuf, line: String, kind: Kind) {
        if let Some(sender) = self.sender.load().as_ref() {
            self.metrics.pending.inc();
            if sender.try_send(Message::Record(path, line, kind)).is_err() {
                self.metrics.pending.dec();
                self.metrics.drop_record(kind);
            }
        } else {
            self.metrics.drop_record(kind);
        }
    }
    pub fn access(&self, path: PathBuf, line: String) {
        self.write(path, line, Kind::Access);
    }

    pub fn shutdown(&self) {
        self.sender.store(None);
        if let Some((done, worker)) = self.worker.lock().unwrap().take() {
            if done.recv_timeout(Duration::from_secs(5)).is_ok() {
                let _ = worker.join();
            } else {
                self.metrics
                    .shutdown_pending
                    .inc_by(self.metrics.pending.get().max(0) as u64);
            }
        }
        signal_hook::low_level::unregister(self.signal);
    }
}
impl Drop for FileLogs {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(
    receiver: Receiver<Message>,
    policy: Arc<ArcSwap<Rotation>>,
    reopen: Arc<AtomicBool>,
    metrics: Metrics,
) {
    let mut files: HashMap<PathBuf, FileSink> = HashMap::new();
    let mut warning = None;
    loop {
        let policy = policy.load_full();
        if reopen.swap(false, Ordering::AcqRel) {
            for (path, file) in &mut files {
                match FileSink::open(path, &policy) {
                    Ok(new) => {
                        *file = new;
                        metrics.reopens.inc();
                    }
                    Err(e) => report(&metrics, &mut warning, "reopen", path, &e),
                }
            }
        }
        let message = match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(message) => message,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let Message::Record(path, line, kind) = message else {
            continue;
        };
        if !files.contains_key(&path) {
            if files.len() >= 128 {
                files.clear();
            }
            match FileSink::open(&path, &policy) {
                Ok(file) => {
                    files.insert(path.clone(), file);
                }
                Err(e) => {
                    report(&metrics, &mut warning, "open", &path, &e);
                    metrics.drop_record(kind);
                    metrics.pending.dec();
                    continue;
                }
            }
        }
        let file = files.get_mut(&path).unwrap();
        file.set_policy(&policy);
        match file.rotate_if_needed(&path, line.len() + 1) {
            Ok(Some(archive)) => {
                metrics.rotations.inc();
                if policy.gzip
                    && let Err(e) = rotation::compress(&archive)
                {
                    report(&metrics, &mut warning, "compress", &path, &e);
                }
                if let Err(e) = rotation::prune(&path, policy.keep) {
                    report(&metrics, &mut warning, "cleanup", &path, &e);
                }
            }
            Ok(None) => {}
            Err(e) => report(&metrics, &mut warning, "rotate", &path, &e),
        }
        if let Err(e) = file.write(&line) {
            report(&metrics, &mut warning, "write", &path, &e);
            metrics.drop_record(kind);
            files.remove(&path);
        }
        metrics.pending.dec();
    }
}

fn report(
    metrics: &Metrics,
    warning: &mut Option<Instant>,
    operation: &str,
    path: &std::path::Path,
    error: &std::io::Error,
) {
    metrics.io_errors.with_label_values(&[operation]).inc();
    if warning.is_none_or(|last| last.elapsed() >= Duration::from_secs(5)) {
        // The file worker must never enqueue its own I/O errors back into itself.
        eprintln!("rgnix log {operation} {}: {error}", path.display());
        *warning = Some(Instant::now());
    }
}
