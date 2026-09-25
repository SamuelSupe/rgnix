use crate::{diagnostics::read_json, runtime::Shared};
use async_trait::async_trait;
use pingora::{apps::http_app::ServeHttp, protocols::http::ServerSession};
use serde_json::{Value, json};
use std::sync::Arc;

pub struct Admission {
    pub shared: Arc<Shared>,
    pub controller_user: Option<String>,
}
#[async_trait]
impl ServeHttp for Admission {
    async fn response(&self, session: &mut ServerSession) -> http::Response<Vec<u8>> {
        session.set_keepalive(None);
        if session.req_header().uri.path() != "/admission/ingress" {
            return reply(404, json!({"error":"unknown admission endpoint"}));
        }
        let input: Value = match read_json(session).await {
            Ok(v) => v,
            Err((status, value)) => return reply(status, value),
        };
        let request = &input["request"];
        let Some(uid) = request["uid"].as_str() else {
            return reply(
                400,
                json!({"error":"AdmissionReview request.uid is required"}),
            );
        };
        let result = self.validate(request).await;
        let (allowed, code, message, warnings) = match result {
            Ok(value) => (true, 200, "accepted".to_owned(), value),
            Err((code, error)) => (false, code, error, vec![]),
        };
        reply(
            200,
            json!({"apiVersion":"admission.k8s.io/v1","kind":"AdmissionReview","response":{"uid":uid,"allowed":allowed,"status":{"code":code,"message":message},"warnings":warnings}}),
        )
    }
}
impl Admission {
    async fn validate(&self, request: &Value) -> Result<Vec<String>, (u16, String)> {
        if request["kind"]["group"] != "networking.k8s.io" || request["kind"]["kind"] != "Ingress" {
            return Err((
                400,
                "webhook only validates networking.k8s.io Ingress".into(),
            ));
        }
        if request["operation"] == "DELETE" {
            return Ok(vec![]);
        }
        if !matches!(request["operation"].as_str(), Some("CREATE" | "UPDATE")) {
            return Err((400, "unsupported admission operation".into()));
        }
        let mut candidate: k8s_openapi::api::networking::v1::Ingress =
            serde_json::from_value(request["object"].clone())
                .map_err(|_| (400, "invalid Ingress object".into()))?;
        if candidate.metadata.namespace.is_none() {
            candidate.metadata.namespace = request["namespace"].as_str().map(str::to_owned);
        }
        if !crate::ingress::managed(&self.shared, &candidate).map_err(|e| (503, e.to_string()))? {
            return Ok(vec![]);
        }
        let old = &request["oldObject"];
        let controlled = ["rgnix.io/rollout-state", "rgnix.io/rolled-back-revision"];
        let changed = controlled.iter().any(|key| {
            old["metadata"]["annotations"][key] != request["object"]["metadata"]["annotations"][key]
        });
        if changed {
            if self
                .controller_user
                .as_ref()
                .is_none_or(|name| request["userInfo"]["username"].as_str() != Some(name))
            {
                return Err((
                    403,
                    "rollout progress is controller-owned; use the authorized rollout API".into(),
                ));
            }
            let mut previous = old["metadata"]["annotations"].clone();
            let mut next = request["object"]["metadata"]["annotations"].clone();
            for key in controlled {
                if let Some(values) = previous.as_object_mut() {
                    values.remove(key);
                }
                if let Some(values) = next.as_object_mut() {
                    values.remove(key);
                }
            }
            // A controller must still retract/roll back traffic when an unrelated application edit is invalid.
            if old["spec"] == request["object"]["spec"] && previous == next {
                return Ok(vec![]);
            }
        }
        let _permit = self
            .shared
            .simulations
            .try_acquire()
            .map_err(|_| (429, "validation capacity exhausted; retry".into()))?;
        let shared = self.shared.clone();
        let result =
            tokio::task::spawn_blocking(move || crate::ingress::validate(&shared, candidate))
                .await
                .map_err(|_| (500, "validation task failed".into()))?
                .map_err(|e| (422, e.to_string()))?;
        if result["valid"] != true {
            return Err((422, result["errors"].to_string()));
        }
        Ok(result["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|w| w.to_string())
            .collect())
    }
}
fn reply(status: u16, body: Value) -> http::Response<Vec<u8>> {
    http::Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&body).unwrap())
        .unwrap()
}
