use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

fn backend(value: &Value, ns: &str, objects: &[Value]) -> Result<Value> {
    ensure!(
        value.get("resource").is_none(),
        "resource backends are unsupported"
    );
    let service = &value["service"];
    let name = service["name"]
        .as_str()
        .context("Service backend name missing")?;
    let number = if let Some(number) = service["port"]["number"].as_u64() {
        number
    } else {
        let port = service["port"]["name"]
            .as_str()
            .context("Service port missing")?;
        objects
            .iter()
            .find(|o| {
                o["kind"] == "Service"
                    && o["metadata"]["name"] == name
                    && o["metadata"]["namespace"].as_str().unwrap_or(ns) == ns
            })
            .and_then(|s| s["spec"]["ports"].as_array())
            .and_then(|ports| ports.iter().find(|p| p["name"] == port))
            .and_then(|p| p["port"].as_u64())
            .context("Named port needs the matching Service manifest in the input")?
    };
    ensure!((1..=65535).contains(&number), "invalid Service port");
    Ok(json!({"name":name,"port":number}))
}

pub(super) fn convert(
    path: &Path,
    output: Option<&Path>,
    class: &str,
    gateway: &str,
    default_ns: &str,
) -> Result<Value> {
    ensure!(
        [class, gateway, default_ns]
            .iter()
            .all(|s| crate::tenancy::namespace_name(s)),
        "invalid class, Gateway or namespace name"
    );
    let bytes = crate::controls::read_bounded(path, 4 * 1024 * 1024)?;
    let mut objects = vec![];
    for document in serde_yaml_ng::Deserializer::from_slice(&bytes) {
        let value = Value::deserialize(document)?;
        if value.is_null() {
            continue;
        }
        if value["kind"] == "List" {
            objects.extend(
                value["items"]
                    .as_array()
                    .context("List.items must be an array")?
                    .clone(),
            );
        } else {
            objects.push(value);
        }
    }
    ensure!(objects.len() <= 4096, "input exceeds 4096 objects");
    for object in &mut objects {
        if (object["kind"] == "Ingress" || object["kind"] == "Service")
            && object["metadata"]["namespace"].is_null()
        {
            object["metadata"]["namespace"] = json!(default_ns);
        }
    }
    let mut findings = vec![];
    let mut generated = vec![
        json!({"apiVersion":"gateway.networking.k8s.io/v1","kind":"GatewayClass","metadata":{"name":class},"spec":{"controllerName":crate::gateway::CONTROLLER}}),
    ];
    let mut listeners = BTreeMap::<String, BTreeMap<String, Value>>::new();
    let ingresses: Vec<_> = objects.iter().filter(|o| o["kind"] == "Ingress").collect();
    ensure!(!ingresses.is_empty(), "no Ingress resources in input");
    let mut defaults = BTreeMap::<String, usize>::new();
    let mut hostless = std::collections::BTreeSet::new();
    let mut claims = std::collections::BTreeSet::new();
    for ingress in &ingresses {
        let ns = ingress["metadata"]["namespace"]
            .as_str()
            .unwrap_or(default_ns);
        if ingress["spec"].get("defaultBackend").is_some() {
            *defaults.entry(ns.into()).or_default() += 1;
        }
        for rule in ingress["spec"]["rules"].as_array().into_iter().flatten() {
            let host = rule["host"].as_str().unwrap_or("");
            if host.is_empty() {
                hostless.insert(ns.to_owned());
            }
            for path in rule["http"]["paths"].as_array().into_iter().flatten() {
                let path_type = path["pathType"].as_str().unwrap_or("");
                let path_value = path["path"].as_str().unwrap_or("/");
                let claim = (
                    ns.to_owned(),
                    host.to_owned(),
                    path_type.to_owned(),
                    if path_type == "Exact" {
                        path_value
                    } else {
                        path_value.trim_end_matches('/')
                    }
                    .to_owned(),
                );
                if !claims.insert(claim) {
                    findings.push(json!({"severity":"blocker","resource":format!("{ns}/{}", ingress["metadata"]["name"].as_str().unwrap_or_default()),"message":"Conflicting source routes need explicit precedence; source creation order cannot be copied to new resources"}));
                }
            }
        }
    }
    for (ns, count) in defaults {
        if count > 1 || hostless.contains(&ns) {
            findings.push(json!({"severity":"blocker","resource":ns,"message":"Default-backend fallback overlaps other default or hostless rules; assign explicit Gateway precedence manually"}));
        }
    }
    for ingress in &ingresses {
        let ns = ingress["metadata"]["namespace"]
            .as_str()
            .unwrap_or(default_ns);
        let name = ingress["metadata"]["name"].as_str().unwrap_or("ingress");
        let identity = format!("{ns}/{name}");
        let result = (|| -> Result<Vec<Value>> {
            ensure!(
                ingress["apiVersion"] == "networking.k8s.io/v1",
                "only networking.k8s.io/v1 Ingress is supported"
            );
            let mut annotations = serde_json::Map::new();
            for (key, value) in ingress["metadata"]["annotations"]
                .as_object()
                .into_iter()
                .flatten()
            {
                match key.as_str() {
                    "kubernetes.io/ingress.class"
                    | "kubectl.kubernetes.io/last-applied-configuration" => {}
                    "rgnix.io/script"
                    | "rgnix.io/request-body"
                    | "rgnix.io/request-body-timeout"
                    | "rgnix.io/client-max-body-size" => {
                        annotations.insert(key.clone(), value.clone());
                    }
                    _ => anyhow::bail!(
                        "annotation {key} has no verified conversion; translate its behavior manually"
                    ),
                }
            }
            let ns_listeners = listeners.entry(ns.into()).or_default();
            ns_listeners.entry("http".into()).or_insert_with(|| json!({"name":"http","protocol":"HTTP","port":80,"allowedRoutes":{"namespaces":{"from":"Same"}}}));
            for tls in ingress["spec"]["tls"].as_array().into_iter().flatten() {
                let secret = tls["secretName"]
                    .as_str()
                    .context("TLS secretName is required")?;
                let hosts = tls["hosts"]
                    .as_array()
                    .context("TLS hosts are required for deterministic certificate selection")?;
                ensure!(!hosts.is_empty(), "TLS hosts cannot be empty");
                for host in hosts {
                    let host = host.as_str().context("invalid TLS hostname")?;
                    ensure!(
                        !host.starts_with("*."),
                        "wildcard Ingress matches one DNS label; Gateway wildcard also matches deeper subdomains"
                    );
                    use sha2::{Digest, Sha256};
                    let listener_name =
                        format!("tls-{:x}", Sha256::digest(host.as_bytes()))[..20].to_owned();
                    let listener = json!({"name":listener_name,"hostname":host,"protocol":"HTTPS","port":443,"tls":{"mode":"Terminate","certificateRefs":[{"name":secret}]},"allowedRoutes":{"namespaces":{"from":"Same"}}});
                    if let Some(previous) = ns_listeners.get(&listener_name) {
                        ensure!(*previous == listener, "conflicting TLS Secrets for {host}");
                    }
                    ns_listeners.insert(listener_name, listener);
                }
            }
            let mut routes = vec![];
            let mut rules = ingress["spec"]["rules"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if let Some(default_backend) = ingress["spec"].get("defaultBackend") {
                rules.push(json!({"http":{"paths":[{"path":"/","pathType":"Prefix","backend":default_backend}]}}));
            }
            for (index, rule) in rules.iter().enumerate() {
                let host = rule["host"].as_str().unwrap_or("");
                ensure!(
                    !host.starts_with("*."),
                    "wildcard Ingress and Gateway hostname semantics differ; explicit hostnames are required"
                );
                let mut gateway_rules = vec![];
                for path in rule["http"]["paths"]
                    .as_array()
                    .context("HTTP paths missing")?
                {
                    let path_type = match path["pathType"].as_str() {
                        Some("Exact") => "Exact",
                        Some("Prefix") => "PathPrefix",
                        Some("ImplementationSpecific") => {
                            findings.push(json!({"severity":"review","resource":identity,"message":"ImplementationSpecific is converted using rgnix's documented segment-prefix semantics; verify the source controller's behavior"}));
                            "PathPrefix"
                        }
                        _ => anyhow::bail!(
                            "pathType must be Exact, Prefix or ImplementationSpecific"
                        ),
                    };
                    gateway_rules.push(json!({"matches":[{"path":{"type":path_type,"value":path["path"].as_str().unwrap_or("/")}}],"backendRefs":[backend(&path["backend"], ns, &objects)?]}));
                }
                let mut spec = json!({"parentRefs":[{"name":gateway}],"rules":gateway_rules});
                if !host.is_empty() {
                    spec["hostnames"] = json!([host]);
                }
                use sha2::{Digest, Sha256};
                let identity_hash = format!("{:x}", Sha256::digest(identity.as_bytes()));
                let route_name = format!(
                    "{}-{}-{index}",
                    name.chars()
                        .take(40)
                        .collect::<String>()
                        .trim_end_matches(['.', '-']),
                    &identity_hash[..10]
                );
                routes.push(json!({"apiVersion":"gateway.networking.k8s.io/v1","kind":"HTTPRoute","metadata":{"name":route_name,"namespace":ns,"annotations":annotations},"spec":spec}));
            }
            Ok(routes)
        })();
        match result {
            Ok(routes) => generated.extend(routes),
            Err(error) => findings.push(
                json!({"severity":"blocker","resource":identity,"message":format!("{error:#}")}),
            ),
        }
    }
    for (ns, listeners) in listeners {
        generated.push(json!({"apiVersion":"gateway.networking.k8s.io/v1","kind":"Gateway","metadata":{"name":gateway,"namespace":ns},"spec":{"gatewayClassName":class,"listeners":listeners.into_values().collect::<Vec<_>>()}}));
    }
    let compatible = !findings.iter().any(|f| f["severity"] == "blocker");
    if compatible && let Some(output) = output {
        super::write_new(
            output,
            serde_yaml_ng::to_string(&json!({"apiVersion":"v1","kind":"List","items":generated}))?
                .as_bytes(),
        )?;
    }
    Ok(
        json!({"kind":"ingress-to-gateway","compatible":compatible,"ingresses":ingresses.len(),"resources":generated.len(),"findings":findings,"output":if compatible {output} else {None},"scope":"Candidate manifests only; existing Services, Secrets and ConfigMaps remain dependencies. Deploy one bound rgnix Gateway data plane per generated Gateway; verify traffic before cutover."}),
    )
}
