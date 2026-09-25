use crate::{
    model::PathMatch,
    script::{Edits, RequestData},
};

pub(crate) fn outbound_uri(
    original: &str,
    request: &RequestData,
    edits: &Edits,
    matcher: &PathMatch,
    proxy_uri: Option<&str>,
) -> String {
    let path = if let Some(path) = &edits.path {
        super::encode_path(path)
    } else if let Some(uri) = proxy_uri {
        let suffix = request
            .path
            .strip_prefix(matcher.path())
            .unwrap_or(&request.path);
        format!("{uri}{}", super::encode_path(suffix))
    } else if edits.query.is_some() {
        original
            .split_once('?')
            .map_or(original, |(path, _)| path)
            .to_owned()
    } else {
        original.to_owned()
    };
    if edits.path.is_some() || edits.query.is_some() || proxy_uri.is_some() {
        let query = edits.query.as_deref().unwrap_or(&request.query);
        if !query.is_empty() && !path.contains('?') {
            return format!("{path}?{query}");
        }
    }
    path
}
pub(crate) struct Variables<'a> {
    pub request: &'a RequestData,
    pub edits: &'a Edits,
    pub original_uri: &'a str,
    pub original_peer: &'a str,
    pub scheme: &'a str,
    pub server_name: &'a str,
}
impl Variables<'_> {
    pub fn expand(&self, input: &str, proxy_host: &str) -> String {
        let mut output = String::new();
        let mut rest = input;
        while let Some(i) = rest.find('$') {
            output.push_str(&rest[..i]);
            rest = &rest[i + 1..];
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            let key = &rest[..end];
            let value = match key {
                "host" if self.request.host.is_empty() => self.server_name.to_owned(),
                "host" => self.request.host.clone(),
                "http_host" => self
                    .request
                    .headers
                    .get("host")
                    .cloned()
                    .unwrap_or_default(),
                "scheme" => {
                    if self.scheme.is_empty() {
                        "http".into()
                    } else {
                        self.scheme.to_owned()
                    }
                }
                "request_uri" => self.original_uri.to_owned(),
                "uri" => self
                    .edits
                    .path
                    .as_ref()
                    .unwrap_or(&self.request.path)
                    .clone(),
                "args" => self
                    .edits
                    .query
                    .as_ref()
                    .unwrap_or(&self.request.query)
                    .clone(),
                "request_method" => self.request.method.clone(),
                "remote_addr" => self.request.remote_addr.clone(),
                "realip_remote_addr" => self.original_peer.to_owned(),
                "proxy_host" => proxy_host.into(),
                "proxy_add_x_forwarded_for" => self
                    .request
                    .headers
                    .get("x-forwarded-for")
                    .map(|v| format!("{v}, {}", self.request.remote_addr))
                    .unwrap_or_else(|| self.request.remote_addr.clone()),
                _ => key
                    .strip_prefix("http_")
                    .and_then(|s| {
                        self.request
                            .headers
                            .get(&s.replace('_', "-").to_ascii_lowercase())
                    })
                    .cloned()
                    .unwrap_or_default(),
            };
            output.push_str(&value);
            rest = &rest[end..];
        }
        output.push_str(rest);
        output
    }
}
