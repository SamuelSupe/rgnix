use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Reference {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteSpec {
    #[serde(default)]
    pub parent_refs: Vec<Reference>,
    #[serde(default)]
    pub hostnames: Vec<String>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rule {
    pub name: Option<String>,
    #[serde(default)]
    pub matches: Vec<Match>,
    #[serde(default)]
    pub filters: Vec<Filter>,
    #[serde(default)]
    pub backend_refs: Vec<BackendRef>,
    pub timeouts: Option<super::timeouts::Timeouts>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Match {
    pub path: Option<Path>,
    pub method: Option<Value>,
    #[serde(default)]
    pub headers: Vec<NamedMatch>,
    #[serde(default)]
    pub query_params: Vec<NamedMatch>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Path {
    #[serde(rename = "type", default = "prefix")]
    pub type_: String,
    #[serde(default = "root")]
    pub value: String,
}
fn prefix() -> String {
    "PathPrefix".into()
}
fn root() -> String {
    "/".into()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NamedMatch {
    #[serde(rename = "type", default = "exact")]
    pub type_: String,
    pub name: String,
    pub value: String,
}
fn exact() -> String {
    "Exact".into()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackendRef {
    pub group: Option<String>,
    pub kind: Option<String>,
    pub name: String,
    pub namespace: Option<String>,
    pub port: Option<u16>,
    pub weight: Option<u32>,
    #[serde(default)]
    pub filters: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Filter {
    #[serde(rename = "type")]
    pub type_: String,
    pub request_header_modifier: Option<Headers>,
    pub response_header_modifier: Option<Headers>,
    pub request_redirect: Option<Redirect>,
    pub url_rewrite: Option<Rewrite>,
    pub request_mirror: Option<Mirror>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Mirror {
    pub backend_ref: BackendRef,
    pub percent: Option<u32>,
    pub fraction: Option<Fraction>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fraction {
    pub numerator: u32,
    pub denominator: Option<u32>,
}

impl Rule {
    pub fn mirror(&self) -> Option<&Mirror> {
        self.filters.iter().find_map(|f| f.request_mirror.as_ref())
    }
    pub fn all_backends(&self) -> impl Iterator<Item = &BackendRef> {
        self.backend_refs
            .iter()
            .chain(self.mirror().map(|m| &m.backend_ref))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Headers {
    #[serde(default)]
    pub set: Vec<Header>,
    #[serde(default)]
    pub add: Vec<Header>,
    #[serde(default)]
    pub remove: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Redirect {
    pub scheme: Option<String>,
    pub hostname: Option<String>,
    pub port: Option<u16>,
    pub path: Option<PathModifier>,
    pub status_code: Option<u16>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rewrite {
    pub hostname: Option<String>,
    pub path: Option<PathModifier>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PathModifier {
    #[serde(rename = "type")]
    pub type_: String,
    pub replace_full_path: Option<String>,
    pub replace_prefix_match: Option<String>,
}
