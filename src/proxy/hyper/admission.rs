use crate::{runtime::Limits, telemetry::Telemetry};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Default)]
struct Counts {
    connections: usize,
    pending: usize,
}

#[derive(Default)]
struct State {
    peers: HashMap<IpAddr, Counts>,
    listeners: HashMap<SocketAddr, usize>,
}

pub(crate) struct Admission {
    connections: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    per_ip: usize,
    per_listener: usize,
    pending_per_ip: usize,
    state: Mutex<State>,
    telemetry: Arc<Telemetry>,
}

pub(super) struct Lease {
    admission: Arc<Admission>,
    peer: IpAddr,
    listener: SocketAddr,
    _connection: OwnedSemaphorePermit,
    pending: AtomicBool,
    pending_permit: Mutex<Option<OwnedSemaphorePermit>>,
}

impl Admission {
    pub(crate) fn new(limits: &Limits, listeners: usize, telemetry: Arc<Telemetry>) -> Arc<Self> {
        let total = limits.hyper_max_connections;
        let automatic_ip = (total / 4).clamp(1, 1024);
        let per_ip = if limits.hyper_max_connections_per_ip == 0 {
            automatic_ip
        } else {
            limits.hyper_max_connections_per_ip.min(total)
        };
        let per_listener = if limits.hyper_max_connections_per_listener == 0 {
            (total / listeners.max(1)).max(1)
        } else {
            limits.hyper_max_connections_per_listener.min(total)
        };
        let pending = if limits.hyper_max_handshakes == 0 {
            (total / 4).clamp(1, 256)
        } else {
            limits.hyper_max_handshakes.min(total)
        };
        let pending_per_ip = if limits.hyper_max_handshakes_per_ip == 0 {
            (per_ip / 2).clamp(1, 16).min(pending)
        } else {
            limits.hyper_max_handshakes_per_ip.min(per_ip).min(pending)
        };
        Arc::new(Self {
            connections: Arc::new(Semaphore::new(total)),
            pending: Arc::new(Semaphore::new(pending)),
            per_ip,
            per_listener,
            pending_per_ip,
            state: Mutex::new(State::default()),
            telemetry,
        })
    }

    pub(super) fn acquire(
        self: &Arc<Self>,
        listener: SocketAddr,
        peer: IpAddr,
    ) -> Result<Arc<Lease>, &'static str> {
        let peer = match peer {
            IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or(IpAddr::V6(ip), IpAddr::V4),
            ip => ip,
        };
        let admitted = || {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state
                .peers
                .get(&peer)
                .is_some_and(|c| c.connections >= self.per_ip)
            {
                return Err("ip");
            }
            if state
                .listeners
                .get(&listener)
                .is_some_and(|n| *n >= self.per_listener)
            {
                return Err("listener");
            }
            if state
                .peers
                .get(&peer)
                .is_some_and(|c| c.pending >= self.pending_per_ip)
            {
                return Err("handshake_ip");
            }
            let connection = self
                .connections
                .clone()
                .try_acquire_owned()
                .map_err(|_| "process")?;
            let pending = self
                .pending
                .clone()
                .try_acquire_owned()
                .map_err(|_| "handshake")?;
            let counts = state.peers.entry(peer).or_default();
            counts.connections += 1;
            counts.pending += 1;
            *state.listeners.entry(listener).or_default() += 1;
            self.telemetry.traffic.hyper_connections.inc();
            self.telemetry.traffic.hyper_pending.inc();
            Ok(Arc::new(Lease {
                admission: self.clone(),
                peer,
                listener,
                _connection: connection,
                pending: AtomicBool::new(true),
                pending_permit: Mutex::new(Some(pending)),
            }))
        };
        let result = admitted();
        if let Err(reason) = result {
            self.telemetry.traffic.hyper_rejected.inc();
            self.telemetry
                .traffic
                .hyper_admission_rejected
                .with_label_values(&[reason])
                .inc();
        }
        result
    }
}

impl Lease {
    pub(super) fn established(&self) {
        if !self.pending.load(Ordering::Acquire) || !self.pending.swap(false, Ordering::AcqRel) {
            return;
        }
        let mut state = self
            .admission
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.peers.get_mut(&self.peer).unwrap().pending -= 1;
        self.admission.telemetry.traffic.hyper_pending.dec();
        self.pending_permit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.established();
        let mut state = self
            .admission
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let counts = state.peers.get_mut(&self.peer).unwrap();
        counts.connections -= 1;
        if counts.connections == 0 {
            state.peers.remove(&self.peer);
        }
        let count = state.listeners.get_mut(&self.listener).unwrap();
        *count -= 1;
        if *count == 0 {
            state.listeners.remove(&self.listener);
        }
        self.admission.telemetry.traffic.hyper_connections.dec();
    }
}

pub(super) fn accept_error(error: &std::io::Error) -> Option<&'static str> {
    use std::io::ErrorKind;
    if matches!(
        error.kind(),
        ErrorKind::Interrupted
            | ErrorKind::WouldBlock
            | ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
    ) {
        return Some("transient");
    }
    #[cfg(unix)]
    match error.raw_os_error() {
        Some(libc::EMFILE | libc::ENFILE | libc::ENOMEM | libc::ENOBUFS) => {
            return Some("resources");
        }
        Some(
            libc::ENETDOWN
            | libc::EPROTO
            | libc::ENOPROTOOPT
            | libc::EHOSTUNREACH
            | libc::ENETUNREACH
            | libc::EOPNOTSUPP,
        ) => return Some("transient"),
        _ => {}
    }
    None
}
