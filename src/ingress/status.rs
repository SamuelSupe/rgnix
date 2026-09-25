use super::{Diagnostic, Options};
use crate::telemetry::Telemetry;
use anyhow::Result;
use futures::{StreamExt, stream};
use k8s_openapi::{
    api::{
        coordination::v1::{Lease, LeaseSpec},
        core::v1::{Event, EventSource, ObjectReference, Service},
        networking::v1::Ingress,
    },
    apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta, Time},
    chrono::{TimeDelta, Utc},
};
use kube::{
    Api, Client, ResourceExt,
    api::{Patch, PatchParams, PostParams},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

pub(super) async fn run(
    client: Client,
    options: Options,
    mut changed: tokio::sync::watch::Receiver<(Vec<Diagnostic>, Vec<Arc<Ingress>>)>,
    telemetry: Arc<Telemetry>,
    mut shutdown: pingora::server::ShutdownWatch,
) {
    let reporter = Reporter::new(client, options, telemetry.clone());
    let mut published_events = BTreeSet::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(10));
    loop {
        tokio::select! {
            _=shutdown.changed()=>break,
            result=changed.changed()=>if result.is_err(){break;},
            _=ticker.tick()=>{},
        }
        let (diagnostics, ingresses) = changed.borrow_and_update().clone();
        let (events, status) = tokio::select! {
            _ = shutdown.changed() => break,
            result = async {
                tokio::join!(
                    tokio::time::timeout(
                        Duration::from_secs(5),
                        reporter.events(diagnostics, &mut published_events),
                    ),
                    reporter.update_status(&ingresses),
                )
            } => result,
        };
        if let Err(e) = events {
            telemetry.report_errors.inc();
            log::warn!("Ingress Event reporting timed out: {e}");
        }
        if let Err(error) = status {
            telemetry.report_errors.inc();
            log::warn!("Ingress status reporting: {error:#}");
        }
    }
}

#[derive(Eq, PartialEq, Ord, PartialOrd)]
struct EventKey {
    namespace: String,
    name: String,
    uid: Option<String>,
    reason: &'static str,
    message: String,
}

pub(super) struct Reporter {
    client: Client,
    options: Options,
    telemetry: Arc<Telemetry>,
}
impl Reporter {
    pub(super) fn new(client: Client, options: Options, telemetry: Arc<Telemetry>) -> Self {
        Self {
            client,
            options,
            telemetry,
        }
    }
    async fn events(&self, diagnostics: Vec<Diagnostic>, published: &mut BTreeSet<EventKey>) {
        let mut active = BTreeMap::new();
        for d in diagnostics {
            let key = EventKey {
                namespace: d.ingress.namespace().unwrap_or_default(),
                name: d.ingress.name_any(),
                uid: d.ingress.uid(),
                reason: d.reason,
                message: d.message.clone(),
            };
            active.insert(key, d);
        }
        published.retain(|key| active.contains_key(key));
        for (key, d) in active {
            if !published.contains(&key) {
                let namespace = &key.namespace;
                log::warn!(
                    "Ingress {namespace}/{}:{}: {}",
                    key.name,
                    d.reason,
                    d.message
                );
                let now = Time(Utc::now());
                let event = Event {
                    metadata: ObjectMeta {
                        generate_name: Some("rgnix-".into()),
                        namespace: Some(namespace.clone()),
                        ..Default::default()
                    },
                    involved_object: ObjectReference {
                        api_version: Some("networking.k8s.io/v1".into()),
                        kind: Some("Ingress".into()),
                        namespace: Some(namespace.clone()),
                        name: Some(d.ingress.name_any()),
                        uid: d.ingress.uid(),
                        ..Default::default()
                    },
                    message: Some(d.message.chars().take(1000).collect()),
                    reason: Some(d.reason.into()),
                    type_: Some("Warning".into()),
                    source: Some(EventSource {
                        component: Some("rgnix".into()),
                        host: Some(self.options.identity.clone()),
                    }),
                    first_timestamp: Some(now.clone()),
                    last_timestamp: Some(now),
                    count: Some(1),
                    ..Default::default()
                };
                match Api::<Event>::namespaced(self.client.clone(), namespace)
                    .create(&PostParams::default(), &event)
                    .await
                {
                    Ok(_) => {
                        // Retain each acknowledgement even if a later write times out.
                        published.insert(key);
                    }
                    Err(e) => {
                        self.telemetry.report_errors.inc();
                        log::warn!("publish Event: {e}");
                    }
                }
            }
        }
    }
    pub(super) async fn leader(&self, timeout: Duration) -> Result<bool> {
        let result = tokio::time::timeout(timeout, self.claim_lease()).await;
        let outcome = match &result {
            Ok(Ok(true)) => "acquired",
            Ok(Ok(false)) => {
                self.telemetry
                    .controller
                    .leader_until
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                "contended"
            }
            Ok(Err(_)) => "error",
            Err(_) => "timeout",
        };
        self.telemetry
            .controller
            .lease_results
            .with_label_values(&[outcome])
            .inc();
        result?
    }
    async fn claim_lease(&self) -> Result<bool> {
        let api = Api::<Lease>::namespaced(self.client.clone(), &self.options.publish_namespace);
        let name = format!("{}-leader", self.options.class);
        let now = Utc::now();
        let current = api.get_opt(&name).await?;
        if let Some(lease) = &current
            && let Some(spec) = &lease.spec
        {
            let fresh = spec.renew_time.as_ref().is_some_and(|t| {
                t.0 + TimeDelta::seconds(i64::from(spec.lease_duration_seconds.unwrap_or(30))) > now
            });
            if fresh && spec.holder_identity.as_deref() != Some(&self.options.identity) {
                return Ok(false);
            }
        }
        let mut lease = current.clone().unwrap_or_else(|| Lease {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                ..Default::default()
            },
            ..Default::default()
        });
        lease.spec = Some(LeaseSpec {
            holder_identity: Some(self.options.identity.clone()),
            lease_duration_seconds: Some(30),
            renew_time: Some(MicroTime(now)),
            ..Default::default()
        });
        let result = if current.is_some() {
            api.replace(&name, &PostParams::default(), &lease).await
        } else {
            api.create(&PostParams::default(), &lease).await
        };
        match result {
            Ok(_) => {
                self.telemetry
                    .controller
                    .leader_until
                    .store(now.timestamp() + 30, std::sync::atomic::Ordering::Relaxed);
                Ok(true)
            }
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    async fn update_status(&self, ingresses: &[Arc<Ingress>]) -> Result<()> {
        let timeout = Duration::from_secs(5);
        if !self.leader(timeout).await? {
            return Ok(());
        }
        let services =
            Api::<Service>::namespaced(self.client.clone(), &self.options.publish_namespace);
        let service =
            tokio::time::timeout(timeout, services.get(&self.options.publish_service)).await??;
        let desired = serde_json::to_value(
            service
                .status
                .and_then(|s| s.load_balancer)
                .unwrap_or_default(),
        )?;
        let updates: Vec<_> = ingresses
            .iter()
            .map(|ingress| {
                let desired = &desired;
                async move {
                    let result =
                        tokio::time::timeout(timeout, self.update_ingress(ingress, desired)).await;
                    if !matches!(result, Ok(Ok(()))) {
                        self.telemetry.report_errors.inc();
                        log::warn!(
                            "Ingress status {}/{}: {result:?}",
                            ingress.namespace().unwrap_or_default(),
                            ingress.name_any()
                        );
                    }
                }
            })
            .collect();
        let mut updates = stream::iter(updates).buffer_unordered(8);
        let period = Duration::from_secs(10);
        let mut renew = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        loop {
            tokio::select! {
                // A large batch must keep renewing its Lease; losing it cancels pending writes.
                _ = renew.tick() => {
                    if !self.leader(timeout).await? {
                        return Ok(());
                    }
                },
                result = updates.next() => if result.is_none() { break; },
            }
        }
        Ok(())
    }

    async fn update_ingress(&self, ingress: &Ingress, desired: &serde_json::Value) -> Result<()> {
        let namespace = ingress.namespace().unwrap_or_default();
        let api = Api::<Ingress>::namespaced(self.client.clone(), &namespace);
        let Some(current) = api.get_opt(&ingress.name_any()).await? else {
            return Ok(());
        };
        if current.uid() != ingress.uid()
            || current
                .spec
                .as_ref()
                .and_then(|s| s.ingress_class_name.as_ref())
                != ingress
                    .spec
                    .as_ref()
                    .and_then(|s| s.ingress_class_name.as_ref())
        {
            return Ok(());
        }
        if serde_json::to_value(
            current
                .status
                .as_ref()
                .and_then(|s| s.load_balancer.as_ref()),
        )? == *desired
        {
            return Ok(());
        }
        // Merge-patch needs an explicit null to withdraw a previously published address.
        let patch = serde_json::json!({"metadata":{"resourceVersion":current.resource_version()},"status":{"loadBalancer":{"ingress":desired.get("ingress")}}});
        match api
            .patch_status(
                &ingress.name_any(),
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 409 || e.code == 404 => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
