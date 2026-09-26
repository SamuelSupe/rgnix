use crate::runtime::Shared;
use anyhow::{Context, Result, ensure};
use k8s_openapi::{
    api::{
        coordination::v1::{Lease, LeaseSpec},
        core::v1::{Pod, Service},
    },
    apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta, OwnerReference},
    chrono::Utc,
};
use kube::{
    Api, Client, ResourceExt,
    api::{ListParams, PostParams},
};
use pingora::server::ShutdownWatch;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

const LABEL: &str = "rgnix.io/fleet";
const REPORT: &str = "rgnix.io/publication";
const FRESH_SECONDS: i64 = 20;

#[derive(Default)]
pub(crate) struct State {
    pub enabled: bool,
    local: Mutex<Local>,
    view: Mutex<Value>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Local {
    desired_sha256: String,
    observed_sha256: String,
    attempt_at: i64,
    completed_at: i64,
    rejection: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Report {
    #[serde(default)]
    rollout_samples: BTreeMap<String, crate::rollout::Samples>,
    pod_uid: String,
    #[serde(flatten)]
    local: Local,
    active_sha256: String,
    controls_sha256: String,
    version: u64,
    ready: bool,
    rejected: i64,
    published_at: i64,
}
impl State {
    pub(crate) fn rollout_samples(
        &self,
        rollout: &crate::rollout::State,
    ) -> Option<crate::rollout::Samples> {
        let view = self.view.lock().unwrap_or_else(|e| e.into_inner());
        let age = Utc::now().timestamp() - view["observed_at"].as_i64()?;
        let freshness =
            (rollout.policy.rollback.as_ref()?.window_seconds as i64).min(FRESH_SECONDS);
        if !(0..=freshness).contains(&age) || !view["error"].is_null() {
            return None;
        }
        let peers = view["replicas"].as_array()?;
        if peers.is_empty() {
            return None;
        }
        let key = rollout.sample_key();
        let mut total = crate::rollout::Samples::default();
        for peer in peers {
            let heartbeat_age = peer["heartbeat_age_seconds"].as_i64()? + age;
            if !(0..=freshness).contains(&heartbeat_age)
                || peer["report"]["ready"] != true
                || matches!(
                    peer["state"].as_str(),
                    Some("missing" | "stale" | "unready")
                )
            {
                return None;
            }
            let sample: crate::rollout::Samples =
                serde_json::from_value(peer["report"]["rollout_samples"][&key].clone()).ok()?;
            total.count = total.count.checked_add(sample.count)?;
            total.errors = total.errors.checked_add(sample.errors)?;
            total.slow = total.slow.checked_add(sample.slow)?;
        }
        Some(total)
    }
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Default::default()
        }
    }
    pub fn begin(&self, digest: String) {
        if !self.enabled {
            return;
        }
        let mut local = self.local.lock().unwrap_or_else(|e| e.into_inner());
        if local.desired_sha256 != digest {
            local.attempt_at = Utc::now().timestamp();
        }
        local.desired_sha256 = digest;
    }
    pub fn complete(&self, rejection: Option<String>) {
        if !self.enabled {
            return;
        }
        let mut local = self.local.lock().unwrap_or_else(|e| e.into_inner());
        local.observed_sha256 = local.desired_sha256.clone();
        local.completed_at = Utc::now().timestamp();
        local.rejection = rejection.map(|value| value.chars().take(512).collect());
    }
    pub fn diagnostic(&self) -> Value {
        let mut value = self.view.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if value.is_null() {
            value = json!({"enabled":self.enabled,"converged":false,"replicas":[],"error":"no replica observation yet"});
        }
        let age = Utc::now().timestamp() - value["observed_at"].as_i64().unwrap_or(0);
        value["observation_age_seconds"] = json!(age.max(0));
        if !(0..=FRESH_SECONDS).contains(&age) {
            value["converged"] = json!(false);
        }
        value
    }
    fn report(&self, shared: &Shared, pod_uid: String) -> Report {
        let snapshot = shared.snapshot.load();
        Report {
            rollout_samples: shared
                .rollouts
                .active()
                .iter()
                .filter(|s| !s.policy.steps.is_empty())
                .take(1024)
                .map(|s| s.sample_report())
                .collect(),
            pod_uid,
            local: self.local.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            active_sha256: snapshot.content_hash.clone(),
            controls_sha256: shared.controls.active.load().digest.clone(),
            version: snapshot.version,
            ready: snapshot.ready && shared.telemetry.draining.get() == 0,
            rejected: shared.telemetry.config_degraded.get(),
            published_at: shared.telemetry.config_updated.get() as i64,
        }
    }
}

pub(crate) fn source_digest<T: Serialize>(source: &T, controls: &str) -> Result<String> {
    let mut value = serde_json::to_value(source)?;
    fn clean(value: &mut Value) {
        match value {
            Value::Object(object) => {
                // API bookkeeping changes do not constitute a new desired configuration.
                if object.contains_key("metadata") {
                    object.remove("status");
                    if let Some(meta) = object.get_mut("metadata").and_then(Value::as_object_mut) {
                        for key in ["resourceVersion", "managedFields"] {
                            meta.remove(key);
                        }
                    }
                }
                for value in object.values_mut() {
                    clean(value);
                }
            }
            Value::Array(values) => {
                for value in values {
                    clean(value);
                }
            }
            _ => {}
        }
    }
    clean(&mut value);
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(value, controls))?)
    ))
}

pub(crate) fn start(
    shared: Arc<Shared>,
    client: Client,
    namespace: String,
    service: String,
    identity: String,
    group: String,
    mut shutdown: ShutdownWatch,
) {
    tokio::spawn(async move {
        let registry = &shared.telemetry.registry;
        let active = prometheus::register_int_gauge_with_registry!(
            "rgnix_fleet_reporting_active",
            "Replica reporting requested or required by staged rollouts",
            registry
        )
        .unwrap();
        let healthy = prometheus::IntGauge::new(
            "rgnix_fleet_observation_healthy",
            "Last replica observation and heartbeat succeeded",
        )
        .unwrap();
        let converged = prometheus::IntGauge::new(
            "rgnix_fleet_converged",
            "All discovered active Pods have fresh matching accepted configurations",
        )
        .unwrap();
        let observed = prometheus::IntGauge::new(
            "rgnix_fleet_observed_timestamp_seconds",
            "Last successful replica observation",
        )
        .unwrap();
        let errors = prometheus::IntCounter::new(
            "rgnix_fleet_errors_total",
            "Replica heartbeat or observation errors",
        )
        .unwrap();
        let replicas = prometheus::IntGaugeVec::new(
            prometheus::Opts::new(
                "rgnix_fleet_replicas",
                "Discovered active Pods by publication state",
            ),
            &["state"],
        )
        .unwrap();
        for metric in [
            Box::new(healthy.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(converged.clone()),
            Box::new(observed.clone()),
            Box::new(errors.clone()),
            Box::new(replicas.clone()),
        ] {
            if let Err(error) = registry.register(metric) {
                log::error!("fleet metrics: {error}");
                return;
            }
        }
        let scope = format!(
            "{:x}",
            Sha256::digest(format!("{namespace}/{service}/{group}"))
        )[..32]
            .to_owned();
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _=shutdown.changed()=>break, _=ticker.tick()=>{} }
            if !shared.fleet.enabled
                && !shared
                    .rollouts
                    .active()
                    .iter()
                    .any(|s| !s.policy.steps.is_empty())
            {
                active.set(0);
                continue;
            }
            active.set(1);
            let result = tokio::time::timeout(
                Duration::from_secs(8),
                observe(&shared, &client, &namespace, &service, &identity, &scope),
            )
            .await;
            match result {
                Ok(Ok(value)) => {
                    healthy.set(1);
                    converged.set(i64::from(value["converged"] == true));
                    observed.set(Utc::now().timestamp());
                    for state in [
                        "converged",
                        "drifted",
                        "rejected",
                        "reconciling",
                        "unready",
                        "stale",
                        "missing",
                    ] {
                        replicas.with_label_values(&[state]).set(
                            value["replicas"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter(|p| p["state"] == state)
                                .count() as i64,
                        );
                    }
                    *shared.fleet.view.lock().unwrap_or_else(|e| e.into_inner()) = value;
                }
                error => {
                    healthy.set(0);
                    converged.set(0);
                    errors.inc();
                    let message = match error {
                        Ok(Err(error)) => error.to_string(),
                        _ => "replica observation timed out".into(),
                    };
                    let mut value = shared.fleet.view.lock().unwrap_or_else(|e| e.into_inner());
                    if value.is_null() {
                        *value = json!({"enabled":true,"replicas":[]});
                    }
                    value["converged"] = json!(false);
                    value["error"] = json!(message);
                }
            }
        }
        healthy.set(0);
        converged.set(0);
    });
}

async fn observe(
    shared: &Shared,
    client: &Client,
    namespace: &str,
    service: &str,
    identity: &str,
    scope: &str,
) -> Result<Value> {
    let service = Api::<Service>::namespaced(client.clone(), namespace)
        .get(service)
        .await?;
    let selector = service
        .spec
        .and_then(|s| s.selector)
        .filter(|s| !s.is_empty())
        .context("publish Service must select controller Pods for replica reporting")?;
    let selector = selector
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    let pods = Api::<Pod>::namespaced(client.clone(), namespace)
        .list(&ListParams::default().labels(&selector).limit(257))
        .await?;
    ensure!(
        pods.items.len() <= 256 && pods.metadata.continue_.as_deref().is_none_or(str::is_empty),
        "replica reporting supports at most 256 selected Pods"
    );
    let own = pods
        .items
        .iter()
        .find(|p| p.name_any() == identity)
        .context("controller identity is not a Pod selected by the publish Service")?;
    let uid = own.uid().context("controller Pod has no UID")?;
    let report = shared.fleet.report(shared, uid.clone());
    let report_json = serde_json::to_string(&report)?;
    ensure!(
        report_json.len() <= 192 * 1024,
        "replica report exceeds 192 KiB; staged rollout paused"
    );
    let api = Api::<Lease>::namespaced(client.clone(), namespace);
    let name = format!(
        "rgnix-replica-{}",
        &format!("{:x}", Sha256::digest(format!("{scope}/{uid}")))[..32]
    );
    let previous = api.get_opt(&name).await?;
    ensure!(
        previous.as_ref().is_none_or(|l| l
            .spec
            .as_ref()
            .and_then(|s| s.holder_identity.as_deref())
            == Some(&uid)),
        "replica Lease is owned by another identity"
    );
    let lease = Lease {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            resource_version: previous.as_ref().and_then(ResourceExt::resource_version),
            labels: Some(BTreeMap::from([(LABEL.into(), scope.into())])),
            annotations: Some(BTreeMap::from([(REPORT.into(), report_json)])),
            owner_references: Some(vec![OwnerReference {
                api_version: "v1".into(),
                kind: "Pod".into(),
                name: identity.into(),
                uid,
                controller: None,
                block_owner_deletion: None,
            }]),
            ..Default::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some(report.pod_uid.clone()),
            renew_time: Some(MicroTime(Utc::now())),
            lease_duration_seconds: Some(FRESH_SECONDS as i32),
            ..Default::default()
        }),
    };
    if previous.is_some() {
        api.replace(&name, &PostParams::default(), &lease).await?;
    } else {
        api.create(&PostParams::default(), &lease).await?;
    }
    let leases = api
        .list(
            &ListParams::default()
                .labels(&format!("{LABEL}={scope}"))
                .limit(1025),
        )
        .await?;
    ensure!(
        leases.items.len() <= 1024
            && leases
                .metadata
                .continue_
                .as_deref()
                .is_none_or(str::is_empty),
        "too many retained replica Leases"
    );
    let now = Utc::now().timestamp();
    let peers: Vec<_> = pods.items.iter().filter(|p| p.metadata.deletion_timestamp.is_none()
        && !p.status.as_ref().and_then(|s| s.phase.as_deref()).is_some_and(|v| v == "Failed" || v == "Succeeded"))
        .map(|pod| {
            let peer = leases.items.iter().find_map(|lease| {
                let spec = lease.spec.as_ref()?;
                if spec.holder_identity.as_deref() != pod.metadata.uid.as_deref() { return None; }
                let text = lease.annotations().get(REPORT)?;
                if text.len() > 192 * 1024 { return None; }
                let peer: Report = serde_json::from_str(text).ok()?;
                if Some(&peer.pod_uid) != pod.metadata.uid.as_ref() { return None; }
                Some((peer, now - spec.renew_time.as_ref()?.0.timestamp()))
            });
            let ready = pod.status.as_ref().and_then(|s| s.conditions.as_ref()).is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
            let state = match &peer {
                None => "missing",
                Some((_, age)) if !(0..=FRESH_SECONDS).contains(age) => "stale",
                Some((p, _)) if !ready || !p.ready => "unready",
                Some((p, _)) if p.rejected > 0 => "rejected",
                Some((p, _)) if p.local.desired_sha256.is_empty() || p.local.desired_sha256 != p.local.observed_sha256 => "reconciling",
                Some((p, _)) if p.active_sha256 != report.active_sha256 || p.controls_sha256 != report.controls_sha256 || p.local.desired_sha256 != report.local.desired_sha256 => "drifted",
                Some(_) => "converged",
            };
            json!({"pod":pod.name_any(),"uid":pod.uid(),"node":pod.spec.as_ref().and_then(|s|s.node_name.as_deref()),"state":state,"heartbeat_age_seconds":peer.as_ref().map(|(_, age)| *age),"report":peer.map(|(p,_)|p)})
        }).collect();
    Ok(
        json!({"enabled":true,"observed_at":now,"target":report,"converged":!peers.is_empty() && peers.iter().all(|p| p["state"] == "converged"),"replicas":peers}),
    )
}

#[derive(clap::Args)]
pub struct WaitOptions {
    #[arg(long)]
    pub admin_url: String,
    #[arg(long)]
    pub token_file: std::path::PathBuf,
    #[arg(long)]
    pub expected_sha256: String,
    #[arg(long, default_value_t = 2)]
    pub replicas: usize,
    #[arg(long, default_value_t = 60)]
    pub timeout_seconds: u64,
}
impl WaitOptions {
    pub fn run(self) -> Result<()> {
        ensure!(
            self.expected_sha256.len() == 64
                && self.expected_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "expected-sha256 requires 64 hexadecimal digits"
        );
        ensure!(
            (1..=256).contains(&self.replicas) && (1..=3600).contains(&self.timeout_seconds),
            "replicas must be 1..256 and timeout-seconds 1..3600"
        );
        let mut url = url::Url::parse(&self.admin_url)?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none(),
            "admin-url requires HTTP(S) without credentials"
        );
        url.set_path("/v1/fleet");
        url.set_query(None);
        url.set_fragment(None);
        let token = crate::controls::read_bounded(&self.token_file, 4098)?;
        let token = std::str::from_utf8(&token)?.trim();
        ensure!(
            (32..=4096).contains(&token.len()),
            "invalid admin token length"
        );
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(self.timeout_seconds);
        loop {
            let response = client.get(url.clone()).bearer_auth(token).send()?;
            ensure!(
                response.status().is_success(),
                "replica status returned HTTP {}",
                response.status()
            );
            use std::io::Read;
            let mut data = vec![];
            response.take(1024 * 1024 + 1).read_to_end(&mut data)?;
            ensure!(data.len() <= 1024 * 1024, "replica response exceeds 1 MiB");
            let value: Value = serde_json::from_slice(&data)?;
            if value["converged"] == true
                && value["target"]["active_sha256"] == self.expected_sha256.to_ascii_lowercase()
                && value["replicas"]
                    .as_array()
                    .is_some_and(|p| p.len() >= self.replicas)
            {
                println!("{}", serde_json::to_string_pretty(&value)?);
                return Ok(());
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "publication did not converge before timeout: {}",
                value
            );
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}
