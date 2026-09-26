use super::{
    build,
    controller::{Options, Resources},
    references::find,
    spec::RouteSpec,
};
use crate::runtime::Shared;
use anyhow::{Context, Result, ensure};
use kube::{ResourceExt, core::DynamicObject};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

#[derive(Clone)]
pub(crate) struct PreviewInput {
    pub(super) options: Options,
    pub(super) resources: Resources,
    pub(super) history: build::History,
}

fn kind(candidate: &DynamicObject) -> Result<&str> {
    let types = candidate
        .types
        .as_ref()
        .context("apiVersion and kind are required")?;
    ensure!(
        types.api_version == "gateway.networking.k8s.io/v1"
            && ["Gateway", "HTTPRoute", "GRPCRoute"].contains(&types.kind.as_str()),
        "expected a v1 Gateway, HTTPRoute or GRPCRoute"
    );
    Ok(&types.kind)
}

pub(crate) fn managed(shared: &Shared, candidate: &DynamicObject) -> Result<bool> {
    let kind = kind(candidate)?;
    let input = shared
        .gateway_preview
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let input = input.as_ref().context("Gateway cache is not ready")?;
    let namespace = candidate.namespace().context("namespace is required")?;
    if kind == "Gateway" {
        return Ok(
            namespace == input.options.namespace && candidate.name_any() == input.options.name
        );
    }
    if !input.options.namespaces.is_empty() && !input.options.namespaces.contains(&namespace) {
        return Ok(false);
    }
    let parents: Vec<super::spec::Reference> =
        serde_json::from_value(candidate.data["spec"]["parentRefs"].clone()).unwrap_or_default();
    Ok(parents
        .iter()
        .any(|p| build::parent_targets(p, &namespace, &input.options)))
}

pub(crate) fn validate(shared: &Shared, mut candidate: DynamicObject) -> Result<Value> {
    let kind = kind(&candidate)?.to_owned();
    ensure!(
        managed(shared, &candidate)?,
        "resource does not target this Gateway or watch scope"
    );
    let mut input = shared
        .gateway_preview
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .context("Gateway cache is not ready")?;
    let active = shared.snapshot.load_full();
    let controls = shared.controls.active.load_full();
    let namespace = candidate.namespace().context("namespace is required")?;
    let name = candidate.name_any();
    ensure!(
        crate::tenancy::namespace_name(&namespace) && !name.is_empty(),
        "namespace/name is required"
    );
    if kind != "Gateway" {
        serde_json::from_value::<RouteSpec>(candidate.data["spec"].clone())?;
    }
    if let Some(previous) = find(&input.resources, &kind, &namespace, &name) {
        candidate.metadata.uid = previous.uid();
        candidate.metadata.creation_timestamp = previous.creation_timestamp();
    } else {
        candidate.metadata.uid = Some(format!("preflight-{namespace}-{kind}-{name}"));
        candidate.metadata.creation_timestamp = Some(
            k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(k8s_openapi::chrono::Utc::now()),
        );
    }
    input.history.strict = candidate.uid();
    let items = input.resources.entry(kind.clone()).or_default();
    items.retain(|o| o.namespace().as_deref() != Some(&namespace) || o.name_any() != name);
    items.push(Arc::new(candidate));
    let isolated = shared.preview();
    let (built, _) = build::build(&input.resources, &input.options, &isolated, input.history);
    let mut errors = vec![];
    let mut reported = false;
    for update in &built.updates {
        if update.kind == kind
            && update.object.namespace().as_deref() == Some(&namespace)
            && update.object.name_any() == name
        {
            reported = true;
            collect_errors(&update.status, &mut errors);
        }
    }
    ensure!(
        reported,
        "Gateway or GatewayClass is unavailable or belongs to another controller"
    );
    ensure!(
        active.version == shared.snapshot.load().version
            && controls.digest == shared.controls.active.load().digest,
        "configuration changed during preflight; retry"
    );
    let scope = crate::diagnostics::auth::Principal {
        name: "preflight".into(),
        role: crate::diagnostics::auth::Role::Reader,
        namespaces: (kind != "Gateway").then(|| BTreeSet::from([namespace])),
    };
    Ok(
        json!({"valid":errors.is_empty(), "errors":errors, "warnings":[], "base_version":active.version,
        "diff":crate::diagnostics::preflight::diff(&scope.view(&active), &scope.view(&built.snapshot))}),
    )
}

fn collect_errors(value: &Value, errors: &mut Vec<Value>) {
    match value {
        Value::Object(object) => {
            if object.get("status").is_some_and(|v| v == "False") {
                errors.push(json!({"condition":value["type"], "reason":value["reason"], "message":value["message"]}));
            }
            for value in object.values() {
                collect_errors(value, errors);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_errors(value, errors);
            }
        }
        _ => {}
    }
}
