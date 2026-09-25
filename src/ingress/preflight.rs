use super::*;
use anyhow::ensure;

#[derive(Clone)]
pub(crate) struct PreviewInput {
    pub(super) options: Options,
    pub(super) resources: Resources,
    pub(super) history: History,
}

pub(crate) fn validate(shared: &Shared, mut candidate: Ingress) -> Result<serde_json::Value> {
    let mut input = shared
        .ingress_preview
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .context("Ingress cache is not ready")?;
    let namespace = candidate
        .namespace()
        .context("Ingress namespace is required")?;
    let name = candidate.name_any();
    ensure!(
        crate::tenancy::namespace_name(&namespace) && !name.is_empty(),
        "Ingress namespace/name is required"
    );
    ensure!(
        input.options.namespaces.is_empty() || input.options.namespaces.contains(&namespace),
        "namespace is outside controller watch scope"
    );
    let spec = candidate
        .spec
        .as_ref()
        .context("Ingress spec is required")?;
    let class = input
        .resources
        .classes
        .iter()
        .find(|c| c.name_any() == input.options.class)
        .context("IngressClass is unavailable")?;
    ensure!(
        class
            .spec
            .as_ref()
            .is_some_and(|s| s.controller.as_deref() == Some(CONTROLLER)),
        "IngressClass belongs to a different controller"
    );
    ensure!(
        spec.ingress_class_name.as_ref().map_or_else(
            || class
                .annotations()
                .get("ingressclass.kubernetes.io/is-default-class")
                .is_some_and(|v| v == "true"),
            |c| c == &input.options.class
        ),
        "Ingress selects a different class"
    );
    if let Some(previous) = input
        .resources
        .ingresses
        .iter()
        .find(|i| i.namespace().as_deref() == Some(&namespace) && i.name_any() == name)
    {
        candidate.metadata.uid = previous.uid();
        candidate.metadata.creation_timestamp = previous.creation_timestamp();
    } else {
        candidate.metadata.uid = Some(format!("preflight-{namespace}-{name}"));
        candidate.metadata.creation_timestamp = Some(
            k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(k8s_openapi::chrono::Utc::now()),
        );
    }
    let active = shared.snapshot.load_full();
    let controls = shared.controls.active.load_full();
    input
        .resources
        .ingresses
        .retain(|i| i.namespace().as_deref() != Some(&namespace) || i.name_any() != name);
    input.resources.ingresses.push(Arc::new(candidate));
    input.resources.prepare(&input.options, &input.history);
    let isolated = shared.preview();
    let (built, _) = build(
        &isolated,
        &input.options,
        input.resources,
        input.history,
        true,
    );
    let (next, diagnostics, _) = built?;
    let diagnostics: Vec<_> = diagnostics
        .iter()
        .filter(|d| {
            d.ingress.namespace().as_deref() == Some(&namespace) && d.ingress.name_any() == name
        })
        .collect();
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| !matches!(d.reason, "UnavailableBackend" | "TrafficRolledBack"))
        .map(|d| serde_json::json!({"reason":d.reason,"message":d.message}))
        .collect();
    let warnings: Vec<_> = diagnostics
        .iter()
        .filter(|d| matches!(d.reason, "UnavailableBackend" | "TrafficRolledBack"))
        .map(|d| serde_json::json!({"reason":d.reason,"message":d.message}))
        .collect();
    ensure!(
        active.version == shared.snapshot.load().version
            && controls.digest == shared.controls.active.load().digest,
        "configuration changed during preflight; retry"
    );
    let scope = crate::diagnostics::auth::Principal {
        name: "preflight".into(),
        role: crate::diagnostics::auth::Role::Reader,
        namespaces: Some(BTreeSet::from([namespace])),
    };
    Ok(
        serde_json::json!({"valid":errors.is_empty(),"errors":errors,"warnings":warnings,"base_version":active.version,"diff":crate::diagnostics::preflight::diff(&scope.view(&active),&scope.view(&next))}),
    )
}

pub(crate) fn managed(shared: &Shared, candidate: &Ingress) -> Result<bool> {
    let input = shared
        .ingress_preview
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let input = input.as_ref().context("Ingress cache is not ready")?;
    if !input.options.namespaces.is_empty()
        && candidate
            .namespace()
            .is_some_and(|ns| !input.options.namespaces.contains(&ns))
    {
        return Ok(false);
    }
    let Some(spec) = &candidate.spec else {
        return Ok(false);
    };
    let Some(class) = input.resources.classes.iter().find(|c| {
        c.name_any() == input.options.class
            && c.spec
                .as_ref()
                .is_some_and(|s| s.controller.as_deref() == Some(CONTROLLER))
    }) else {
        return Ok(false);
    };
    Ok(spec.ingress_class_name.as_ref().map_or_else(
        || {
            class
                .annotations()
                .get("ingressclass.kubernetes.io/is-default-class")
                .is_some_and(|v| v == "true")
        },
        |c| c == &input.options.class,
    ))
}
