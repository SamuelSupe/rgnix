use crate::config::syntax::Directive;
use anyhow::Result;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

const SUPPORTED: &str = "http server upstream events listen server_name location root alias try_files index types default_type proxy_http_version proxy_ssl_certificate proxy_ssl_certificate_key proxy_ssl_name proxy_ssl_trusted_certificate proxy_ssl_verify proxy_ssl_server_name proxy_pass return proxy_set_header add_header client_max_body_size rgnix_request_body rgnix_request_body_timeout proxy_connect_timeout proxy_read_timeout proxy_send_timeout keepalive_timeout ssl_certificate ssl_certificate_key http2 rgnix_script access_log rgnix_log_rotation error_log set_real_ip_from real_ip_header real_ip_recursive allow deny least_conn ip_hash rgnix_balance rgnix_max_inflight rgnix_health_check rgnix_limit_rate rgnix_limit_conn gzip gzip_types gzip_min_length gzip_comp_level brotli brotli_comp_level rgnix_auth_request rgnix_jwt ssl_client_certificate ssl_verify_client rgnix_log_query rgnix_log_client rgnix_log_referer rgnix_log_redact rgnix_log_fields";
const PROCESS: &[&str] = &[
    "user",
    "worker_processes",
    "worker_connections",
    "pid",
    "daemon",
    "master_process",
    "worker_rlimit_nofile",
    "multi_accept",
    "use",
];

fn walk(
    nodes: &mut Vec<Directive>,
    context: &str,
    accept: bool,
    inventory: &mut BTreeMap<String, usize>,
    findings: &mut Vec<Value>,
    base: &Path,
) {
    nodes.retain_mut(|node| {
        *inventory.entry(node.name.clone()).or_default() += 1;
        let is_process = PROCESS.contains(&node.name.as_str()) && (context == "main" || context == "events");
        if is_process {
            findings.push(json!({"severity":if accept {"warning"} else {"blocker"},"source":node.source,"directive":node.name,"message":"NGINX process model has no configuration equivalent; use rgnix CLI and container resource limits"}));
            return !accept;
        }
        if context != "types" && !SUPPORTED.split_whitespace().any(|name| name == node.name) {
            findings.push(json!({"severity":"blocker","source":node.source,"directive":node.name,"message":"Directive is not in the migration compatibility inventory; manual review required"}));
        }
        if node.name == "location" && (node.args.first().is_some_and(|s| ["~", "~*", "^~", "@"].contains(&s.as_str())) || context == "location") {
            findings.push(json!({"severity":"blocker","source":node.source,"directive":node.name,"message":"Regular expression, named and nested locations cannot be converted automatically"}));
        }
        if ["proxy_pass", "proxy_set_header", "add_header", "location", "root", "try_files"].contains(&node.name.as_str()) {
            findings.push(json!({"severity":"review","source":node.source,"directive":node.name,"message":"Validate URI, header inheritance and file semantics with request fixtures; syntax acceptance does not prove NGINX equivalence"}));
        }
        if ["root", "alias", "ssl_certificate", "ssl_certificate_key", "rgnix_script", "proxy_ssl_certificate", "proxy_ssl_certificate_key", "proxy_ssl_trusted_certificate", "ssl_client_certificate", "access_log", "error_log"].contains(&node.name.as_str())
            && let Some(path) = node.args.first_mut()
            && !["off", "stderr"].contains(&path.as_str()) && !path.contains('$') {
                *path = base.join(&*path).to_string_lossy().into_owned();
            }
        if let Some(children) = &mut node.children { walk(children, &node.name, accept, inventory, findings, base); }
        if node.name == "rgnix_jwt" {
            for argument in &mut node.args {
                if let Some(path) = argument.strip_prefix("jwks=") && !path.starts_with("https://") {
                    *argument = format!("jwks={}", base.join(path).display());
                }
            }
        }
        true
    });
}
fn render(nodes: &[Directive], depth: usize, output: &mut String) {
    for node in nodes {
        output.push_str(&"    ".repeat(depth));
        output.push_str(&node.name);
        for argument in &node.args {
            output.push_str(" \"");
            output.push_str(
                &argument
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
                    .replace('\t', "\\t"),
            );
            output.push('"');
        }
        if let Some(children) = &node.children {
            output.push_str(" {\n");
            render(children, depth + 1, output);
            output.push_str(&"    ".repeat(depth));
            output.push_str("}\n");
        } else {
            output.push_str(";\n");
        }
    }
}
pub(super) fn assess(path: &Path, output: Option<&Path>, accept: bool) -> Result<Value> {
    let path = path.canonicalize()?;
    let mut nodes = crate::config::syntax::load(&path)?;
    if let Some(http) = nodes.iter_mut().find(|n| n.name == "http")
        && let Some(children) = &mut http.children
    {
        children.insert(
            0,
            Directive {
                name: "root".into(),
                args: vec![
                    path.parent()
                        .unwrap()
                        .join("html")
                        .to_string_lossy()
                        .into_owned(),
                ],
                children: None,
                source: "migration: explicit original default root".into(),
            },
        );
    }
    let mut inventory = BTreeMap::new();
    let mut findings = vec![];
    walk(
        &mut nodes,
        "main",
        accept,
        &mut inventory,
        &mut findings,
        path.parent().unwrap(),
    );
    let mut candidate = String::new();
    render(&nodes, 0, &mut candidate);
    let mut compatible = !findings.iter().any(|f| f["severity"] == "blocker");
    if compatible
        && let Err(error) = crate::config::load_nodes(
            &nodes,
            path.parent().unwrap(),
            &crate::script::Compiler::new()?,
            1,
        )
    {
        compatible = false;
        findings.push(json!({"severity":"blocker","source":path,"message":format!("Candidate validation: {error:#}")}));
    }
    if compatible && let Some(output) = output {
        super::write_new(output, candidate.as_bytes())?;
    }
    Ok(
        json!({"kind":"nginx-migration","compatible":compatible,"inventory":inventory,"findings":findings,"output":if compatible {output} else {None},"scope":"Static assessment and rgnix validation; run NGINX differential tests before cutover"}),
    )
}
