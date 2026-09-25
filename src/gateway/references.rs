use super::{GROUP, controller::Resources};
use crate::model::{Certificate, RuntimeSnapshot, TlsHost};
use anyhow::{Context, Result, ensure};
use kube::{ResourceExt, core::DynamicObject};
use serde_json::Value;
use std::sync::Arc;

pub(super) fn objects<'a>(resources: &'a Resources, kind: &str) -> &'a [Arc<DynamicObject>] {
    resources.get(kind).map(Vec::as_slice).unwrap_or(&[])
}
pub(super) fn find<'a>(
    resources: &'a Resources,
    kind: &str,
    ns: &str,
    name: &str,
) -> Option<&'a Arc<DynamicObject>> {
    objects(resources, kind)
        .iter()
        .find(|o| o.name_any() == name && o.namespace().as_deref().unwrap_or("") == ns)
}
pub(super) fn text<'a>(value: &'a Value, name: &str, fallback: &'a str) -> &'a str {
    value.get(name).and_then(Value::as_str).unwrap_or(fallback)
}
pub(super) fn permitted(
    resources: &Resources,
    from_ns: &str,
    from_kind: &str,
    to_ns: &str,
    to_kind: &str,
    name: &str,
) -> bool {
    if from_ns == to_ns {
        return true;
    }
    objects(resources, "ReferenceGrant")
        .iter()
        .filter(|g| g.namespace().as_deref() == Some(to_ns))
        .any(|grant| {
            let spec = &grant.data["spec"];
            spec["from"].as_array().is_some_and(|from| {
                from.iter().any(|f| {
                    text(f, "group", "") == GROUP
                        && text(f, "kind", "") == from_kind
                        && text(f, "namespace", "") == from_ns
                })
            }) && spec["to"].as_array().is_some_and(|to| {
                to.iter().any(|t| {
                    text(t, "group", "").is_empty()
                        && text(t, "kind", "") == to_kind
                        && t.get("name").is_none_or(|n| n.as_str() == Some(name))
                })
            })
        })
}

pub(super) fn allows(
    resources: &Resources,
    listener: &Value,
    gateway_ns: &str,
    route_ns: &str,
    kind: &str,
) -> bool {
    let allowed = &listener["allowedRoutes"];
    if let Some(kinds) = allowed["kinds"].as_array()
        && !kinds
            .iter()
            .any(|k| text(k, "group", GROUP) == GROUP && text(k, "kind", "") == kind)
    {
        return false;
    }
    match text(&allowed["namespaces"], "from", "Same") {
        "Same" => gateway_ns == route_ns,
        "All" => true,
        "Selector" => find(resources, "Namespace", "", route_ns).is_some_and(|ns| {
            let selector = &allowed["namespaces"]["selector"];
            let labels = ns.labels();
            selector.is_object()
                && selector["matchLabels"].as_object().is_none_or(|wanted| {
                    wanted
                        .iter()
                        .all(|(key, value)| labels.get(key).map(String::as_str) == value.as_str())
                })
                && selector["matchExpressions"]
                    .as_array()
                    .is_none_or(|expressions| {
                        expressions.iter().all(|expression| {
                            let key = text(expression, "key", "");
                            let value = labels.get(key);
                            let contains = value.is_some_and(|v| {
                                expression["values"].as_array().is_some_and(|vs| {
                                    vs.iter().any(|wanted| wanted.as_str() == Some(v))
                                })
                            });
                            match text(expression, "operator", "") {
                                "In" => contains,
                                "NotIn" => !contains,
                                "Exists" => value.is_some(),
                                "DoesNotExist" => value.is_none(),
                                _ => false,
                            }
                        })
                    })
        }),
        _ => false,
    }
}

pub(super) fn certificate(
    resources: &Resources,
    gateway: &DynamicObject,
    listener: &Value,
    snapshot: &mut RuntimeSnapshot,
    address: std::net::SocketAddr,
) -> Result<()> {
    let hostname = text(listener, "hostname", "");
    let mut tls_host = TlsHost {
        listener: address,
        name: hostname.into(),
        ingress: false,
        default: hostname.is_empty(),
        certificate: None,
        client_auth: Default::default(),
    };
    let result = (|| {
        ensure!(
            listener["tls"]
                .as_object()
                .is_some_and(|tls| tls
                    .keys()
                    .all(|key| ["mode", "certificateRefs"].contains(&key.as_str()))),
            "unsupported TLS field; frontend validation and TLS options are not implemented"
        );
        ensure!(
            text(&listener["tls"], "mode", "Terminate") == "Terminate",
            "only TLS termination is supported"
        );
        ensure!(
            listener["tls"].get("options").is_none(),
            "TLS options are unsupported"
        );
        let refs = listener["tls"]["certificateRefs"]
            .as_array()
            .context("certificateRefs required")?;
        ensure!(
            refs.len() == 1,
            "exactly one certificate reference is supported per listener"
        );
        let reference = &refs[0];
        ensure!(
            text(reference, "group", "").is_empty()
                && text(reference, "kind", "Secret") == "Secret",
            "certificate must reference a Secret"
        );
        let gateway_ns = gateway.namespace().unwrap_or_default();
        let ns = text(reference, "namespace", &gateway_ns);
        let name = text(reference, "name", "");
        ensure!(
            permitted(resources, &gateway_ns, "Gateway", ns, "Secret", name),
            "RefNotPermitted"
        );
        let secret =
            find(resources, "Secret", ns, name).context("InvalidCertificateRef: Secret missing")?;
        let secret: k8s_openapi::api::core::v1::Secret =
            serde_json::from_value(serde_json::to_value(secret)?)?;
        ensure!(
            secret.type_.as_deref() == Some("kubernetes.io/tls"),
            "Secret type must be kubernetes.io/tls"
        );
        let data = secret.data.context("empty TLS Secret")?;
        let cert = Certificate::parse(
            &data.get("tls.crt").context("tls.crt missing")?.0,
            &data.get("tls.key").context("tls.key missing")?.0,
        )?;
        if !hostname.is_empty() {
            cert.validate_name(hostname)?;
        }
        tls_host.certificate = Some(Arc::new(cert));
        Ok(())
    })();
    snapshot.certificates.push(tls_host);
    result
}
