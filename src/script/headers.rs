use std::collections::BTreeMap;

/// An immutable HTTP header view until authentication needs to change it.
/// Standalone requests retain the last valid UTF-8 value for repeated fields,
/// matching their existing map projection. Gateway inputs are normalized first.
#[derive(Clone, Debug, Default)]
pub struct RequestHeaders {
    source: Option<http::HeaderMap>,
    values: BTreeMap<String, String>,
}

impl RequestHeaders {
    pub fn from_http(headers: &http::HeaderMap) -> Self {
        Self {
            source: Some(headers.clone()),
            values: BTreeMap::new(),
        }
    }

    #[cfg(feature = "hyper-experimental")]
    pub(crate) fn from_selected(headers: &http::HeaderMap, names: &[http::HeaderName]) -> Self {
        let mut selected = http::HeaderMap::new();
        for name in names {
            for value in headers.get_all(name) {
                selected.append(name.clone(), value.clone());
            }
        }
        Self {
            source: Some(selected),
            values: BTreeMap::new(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        match &self.source {
            Some(headers) => headers
                .get_all(name)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .next_back(),
            None => self.values.get(name).map(String::as_str),
        }
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .chain(self.source.iter().flat_map(|headers| {
                headers
                    .keys()
                    .filter_map(|name| self.get(name.as_str()).map(|v| (name.as_str(), v)))
            }))
    }

    pub fn to_map(&self) -> BTreeMap<String, String> {
        self.iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    pub fn host_bytes(&self) -> usize {
        // The HTTP view also retains duplicates and non-UTF-8 values that RGL
        // cannot read. Charge those bytes to the host budget as well.
        match &self.source {
            Some(headers) => headers
                .iter()
                .map(|(k, v)| k.as_str().len() + v.len() + 64)
                .sum(),
            None => self
                .values
                .iter()
                .map(|(k, v)| k.len() + v.len() + 64)
                .sum(),
        }
    }

    fn materialize(&mut self) {
        if self.source.is_some() {
            self.values = self.to_map();
            self.source = None;
        }
    }

    pub fn insert(&mut self, name: String, value: String) {
        self.materialize();
        self.values.insert(name, value);
    }

    pub fn remove(&mut self, name: &str) {
        self.materialize();
        self.values.remove(name);
    }
}

impl FromIterator<(String, String)> for RequestHeaders {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        Self {
            source: None,
            values: iter.into_iter().collect(),
        }
    }
}

impl From<BTreeMap<String, String>> for RequestHeaders {
    fn from(values: BTreeMap<String, String>) -> Self {
        Self {
            source: None,
            values,
        }
    }
}

impl Extend<(String, String)> for RequestHeaders {
    fn extend<T: IntoIterator<Item = (String, String)>>(&mut self, iter: T) {
        self.materialize();
        self.values.extend(iter);
    }
}
