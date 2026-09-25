use super::{
    syntax::{self, Directive},
    *,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub nodes: Vec<Directive>,
    #[serde(with = "assets")]
    pub assets: BTreeMap<String, Vec<u8>>,
}
impl Bundle {
    pub fn capture(path: &Path) -> Result<Self> {
        let path = path.canonicalize()?;
        let base = path.parent().unwrap();
        let mut nodes = syntax::load(&path)?;
        let mut bundle = Self {
            nodes: Vec::new(),
            assets: BTreeMap::new(),
        };
        fn visit(nodes: &mut [Directive], base: &Path, bundle: &mut Bundle) -> Result<()> {
            for node in nodes {
                if let Some(children) = &mut node.children {
                    visit(children, base, bundle)?;
                }
                let asset = matches!(
                    node.name.as_str(),
                    "ssl_certificate"
                        | "ssl_certificate_key"
                        | "ssl_client_certificate"
                        | "proxy_ssl_certificate"
                        | "proxy_ssl_certificate_key"
                        | "proxy_ssl_trusted_certificate"
                        | "rgnix_script"
                );
                if asset && node.args.len() == 1 && node.args[0] != "off" {
                    let original = base.join(&node.args[0]);
                    let ext = original
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("bin");
                    let name = format!("asset-{}.{}", bundle.assets.len(), ext);
                    let bytes =
                        std::fs::read(&original).with_context(|| original.display().to_string())?;
                    bundle.assets.insert(name.clone(), bytes);
                    node.args[0] = name;
                }
                if node.name == "rgnix_jwt" {
                    for arg in &mut node.args {
                        if let Some(path) = arg.strip_prefix("jwks=")
                            && !path.starts_with("https://")
                        {
                            let name = format!("asset-{}.json", bundle.assets.len());
                            bundle
                                .assets
                                .insert(name.clone(), std::fs::read(base.join(path))?);
                            *arg = format!("jwks={name}");
                        }
                    }
                }
                if matches!(
                    node.name.as_str(),
                    "root" | "alias" | "access_log" | "error_log"
                ) && let Some(path) = node.args.first_mut()
                    && path != "off"
                    && path != "stderr"
                {
                    *path = base.join(&*path).to_string_lossy().into();
                }
                ensure!(
                    bundle.assets.values().map(Vec::len).sum::<usize>() <= 16 * 1024 * 1024,
                    "configuration assets exceed 16 MiB"
                );
            }
            Ok(())
        }
        visit(&mut nodes, base, &mut bundle)?;
        if let Some(http) = nodes.iter_mut().find(|n| n.name == "http")
            && let Some(children) = &mut http.children
        {
            children.insert(
                0,
                Directive {
                    name: "root".into(),
                    args: vec![base.join("html").to_string_lossy().into()],
                    children: None,
                    source: path.display().to_string(),
                },
            );
        }
        bundle.nodes = nodes;
        Ok(bundle)
    }
    pub fn config_bytes(&self) -> usize {
        serde_json::to_vec(&self.nodes).unwrap().len()
    }
    pub fn compile(self: &Arc<Self>, compiler: &Compiler, version: u64) -> Result<RuntimeSnapshot> {
        ensure!(
            self.config_bytes() <= 4 * 1024 * 1024
                && self.assets.values().map(Vec::len).sum::<usize>() <= 16 * 1024 * 1024,
            "stored configuration exceeds limits"
        );
        let temp = tempfile::tempdir()?;
        for (name, bytes) in &self.assets {
            ensure!(
                !name.contains(['/', '\\']) && name.starts_with("asset-"),
                "invalid asset path"
            );
            std::fs::write(temp.path().join(name), bytes)?;
        }
        let mut snapshot = super::load_nodes(&self.nodes, temp.path(), compiler, version)?;
        snapshot.source_bundle = Some(self.clone());
        Ok(snapshot)
    }
}

mod assets {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        value: &BTreeMap<String, Vec<u8>>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(value.len()))?;
        for (name, bytes) in value {
            map.serialize_entry(name, &openssl::base64::encode_block(bytes))?;
        }
        map.end()
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<BTreeMap<String, Vec<u8>>, D::Error> {
        let map = BTreeMap::<String, String>::deserialize(deserializer)?;
        map.into_iter()
            .map(|(name, value)| {
                openssl::base64::decode_block(&value)
                    .map(|bytes| (name, bytes))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}
