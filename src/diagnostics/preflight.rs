use super::*;
use sha2::{Digest, Sha256};

pub fn diff(before: &RuntimeSnapshot, after: &RuntimeSnapshot) -> Value {
    fn entries(snapshot: &RuntimeSnapshot) -> BTreeMap<String, (String, Value)> {
        snapshot.hosts.iter().flat_map(|host|host.routes.iter().map(move |route| {
            let key=format!("{}|{}|{}",host.listener,host.names.join(","),route.id);
            let value=json!({"settings":route.settings,"action":route.action,"script":route.script.as_ref().map(|s|&s.digest),"backends":route.allowed_backends,"rollout":route.rollout.as_ref().map(|r|&r.policy)});
            let hash=format!("{:x}",Sha256::digest(serde_json::to_vec(&value).unwrap()));
            (key,(hash,super::route(route)))
        })).collect()
    }
    let previous = entries(before);
    let next = entries(after);
    let added: Vec<_> = next
        .iter()
        .filter(|(id, _)| !previous.contains_key(*id))
        .map(|(id, (_, value))| json!({"id":id,"after":value}))
        .collect();
    let removed: Vec<_> = previous
        .iter()
        .filter(|(id, _)| !next.contains_key(*id))
        .map(|(id, (_, value))| json!({"id":id,"before":value}))
        .collect();
    let changed: Vec<_> = next
        .iter()
        .filter_map(|(id, (hash, value))| {
            previous
                .get(id)
                .filter(|(old, _)| old != hash)
                .map(|(_, old)| json!({"id":id,"before":old,"after":value}))
        })
        .collect();
    let backend_names: std::collections::BTreeSet<_> = before
        .backends
        .keys()
        .chain(after.backends.keys())
        .collect();
    let backend_changes: Vec<_>=backend_names.into_iter().filter(|name|match (before.backends.get(*name),after.backends.get(*name)) {(Some(a),Some(b))=>!a.same_config(b),_=>true}).map(|name|json!({"backend":name,"before":before.backends.get(name).map(|b|b.diagnostic()),"after":after.backends.get(name).map(|b|b.diagnostic())})).collect();
    json!({"backend_changes":backend_changes,"added":added,"removed":removed,"changed":changed,"restart_required":before.listeners!=after.listeners,"certificate_changes":certificate_ids(before)!=certificate_ids(after)})
}
fn certificate_ids(snapshot: &RuntimeSnapshot) -> Vec<(String, String)> {
    snapshot
        .certificates
        .iter()
        .map(|c| {
            (
                format!("{}|{}", c.listener, c.name),
                c.certificate
                    .as_ref()
                    .and_then(|c| c.leaf.to_der().ok())
                    .map_or(String::new(), |der| format!("{:x}", Sha256::digest(der))),
            )
        })
        .collect()
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRequest {
    pub config_path: PathBuf,
    pub expected_version: u64,
}
impl Admin {
    pub(super) async fn validate(
        &self,
        session: &mut ServerSession,
        principal: &auth::Principal,
    ) -> (u16, Value) {
        let Ok(_permit) = self.shared.simulations.try_acquire() else {
            return (429, json!({"error":"preflight budget exhausted"}));
        };
        if session.req_header().uri.path() == "/v1/validate-ingress" {
            let candidate: k8s_openapi::api::networking::v1::Ingress =
                match read_json(session).await {
                    Ok(v) => v,
                    Err(e) => return e,
                };
            if !candidate
                .metadata
                .namespace
                .as_deref()
                .is_some_and(|ns| principal.allows(ns))
            {
                return (403, json!({"error":"namespace outside identity scope"}));
            }
            let shared = self.shared.clone();
            return match tokio::task::spawn_blocking(move || {
                crate::ingress::validate(&shared, candidate)
            })
            .await
            {
                Ok(Ok(result)) => (200, result),
                Ok(Err(e)) => (400, json!({"error":e.to_string()})),
                Err(_) => (500, json!({"error":"preflight task failed"})),
            };
        }
        if !self.shared.file_mode || !principal.global() || !principal.writer() {
            return (
                403,
                json!({"error":"file preflight requires a global writer in standalone mode"}),
            );
        }
        let request: FileRequest = match read_json(session).await {
            Ok(v) => v,
            Err(e) => return e,
        };
        let current = self.shared.snapshot.load_full();
        if request.expected_version != current.version {
            return (409, json!({"error":"configuration version changed"}));
        }
        let compiler = self.shared.compiler.clone();
        let base_version = current.version;
        match tokio::task::spawn_blocking(move||crate::config::load(&request.config_path,&compiler,current.version).map(|next|json!({"valid":true,"base_version":current.version,"diff":diff(&current,&next)}))).await {
            Ok(Ok(_)) if self.shared.snapshot.load().version != base_version => (409, json!({"error":"configuration changed during preflight; retry"})),
            Ok(Ok(value))=>(200,value),
            Ok(Err(e))=>(400,json!({"valid":false,"error":e.to_string()})),
            Err(_)=>(500,json!({"error":"preflight task failed"})),
        }
    }
}
