mod budget;
mod codegen;
mod json;
mod queue;
pub(crate) mod syntax;
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    io::Read,
    path::Path,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Instant,
};
use wasmtime::{
    Caller, Config, Engine, Instance, InstanceAllocationStrategy, InstancePre, Linker, Module,
    PoolingAllocationConfig, Store, StoreLimits, StoreLimitsBuilder, Strategy, Val,
};

pub use codegen::compile;
const HOST_LIMIT: usize = 1024 * 1024;
const FUEL: u64 = 100_000;
const MEMORY_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct PendingCompilation(pub &'static str);
impl std::fmt::Display for PendingCompilation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}
impl std::error::Error for PendingCompilation {}

pub struct Compiler {
    pub(crate) changed: tokio::sync::Notify,
    pub(crate) generation: std::sync::atomic::AtomicU64,
    engine: Engine,
    cache: Mutex<HashMap<[u8; 32], Cached>>,
    build_lock: Mutex<()>,
    queue: OnceLock<Arc<queue::Queue>>,
    warm: Mutex<VecDeque<(Instant, Arc<CompiledScript>)>>,
}
enum Cached {
    Ready(Weak<CompiledScript>),
    Rejected(String),
}
pub struct CompiledScript {
    pub digest: String,
    engine: Engine,
    instance_pre: InstancePre<Host>,
}
#[derive(Clone, Debug, Default)]
pub struct RequestData {
    pub claims: BTreeMap<String, String>,
    pub method: String,
    pub path: String,
    pub query: String,
    pub host: String,
    pub remote_addr: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<crate::body::BodyView>,
}
#[derive(Clone, Debug, Default)]
pub struct Edits {
    pub path: Option<String>,
    pub query: Option<String>,
    pub headers: BTreeMap<String, Option<String>>,
}
#[derive(Clone, Debug, Default)]
pub enum Decision {
    #[default]
    Pass,
    Proxy(String),
    Reply(u16, String),
}
#[derive(Clone, Debug, Default)]
pub struct RequestOutcome {
    pub edits: Edits,
    pub decision: Decision,
}
pub struct Execution {
    store: Store<Host>,
    instance: Instance,
}
struct Host {
    request: RequestData,
    outcome: RequestOutcome,
    decision_set: bool,
    response: bool,
    status: u16,
    response_headers: BTreeMap<String, String>,
    response_edits: BTreeMap<String, Option<String>>,
    strings: Vec<String>,
    allocated: usize,
    limits: StoreLimits,
}

impl Compiler {
    pub fn new() -> Result<Self> {
        Self::with_pool(None)
    }
    /// Size the Wasm resource pool to the HTTP admission budget. Stores, host
    /// data and guest state remain private to each execution.
    pub fn for_runtime(max_plugin_instances: usize) -> Result<Self> {
        ensure!(
            (1..=1_000_000).contains(&max_plugin_instances),
            "max-plugin-instances must be 1..1000000"
        );
        // Two simulator permits and one serialized compilation check may run
        // while every HTTP plugin permit is occupied.
        Self::with_pool(Some(max_plugin_instances as u32 + 3))
    }
    fn with_pool(capacity: Option<u32>) -> Result<Self> {
        let mut config = Config::new();
        config
            .strategy(Strategy::Cranelift)
            .consume_fuel(true)
            .max_wasm_stack(256 * 1024);
        if let Some(capacity) = capacity {
            let mut pool = PoolingAllocationConfig::new();
            pool.total_core_instances(capacity)
                .total_memories(capacity)
                .max_memories_per_module(1)
                .max_memory_size(MEMORY_LIMIT)
                .total_tables(0)
                .max_tables_per_module(0)
                .max_unused_warm_slots(0)
                .linear_memory_keep_resident(64 * 1024);
            config
                .memory_reservation(MEMORY_LIMIT as u64)
                .memory_guard_size(64 * 1024)
                .allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
        }
        Ok(Self {
            changed: tokio::sync::Notify::new(),
            generation: std::sync::atomic::AtomicU64::new(0),
            engine: Engine::new(&config)?,
            cache: Mutex::new(HashMap::new()),
            build_lock: Mutex::new(()),
            queue: OnceLock::new(),
            warm: Mutex::new(VecDeque::new()),
        })
    }
    pub fn load(&self, path: &Path) -> Result<Arc<CompiledScript>> {
        let bytes = read_input(path, 2 * 1024 * 1024)?;
        self.from_bytes(&bytes, path.extension().is_some_and(|s| s == "wasm"))
            .with_context(|| path.display().to_string())
    }
    pub fn from_bytes(&self, bytes: &[u8], wasm: bool) -> Result<Arc<CompiledScript>> {
        ensure!(bytes.len() <= 2 * 1024 * 1024, "plugin input exceeds 2 MiB");
        let mut digest = Sha256::new();
        digest.update([u8::from(wasm)]);
        digest.update(bytes);
        let key = digest.finalize().into();
        if let Some(result) = self.cached(&key)? {
            return result;
        }
        let _build = self
            .build_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("compiler lock poisoned"))?;
        if let Some(result) = self.cached(&key)? {
            return result;
        }
        let result = self.build(bytes, wasm);
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("compiler lock poisoned"))?;
        if let Some(Cached::Rejected(message)) = cache.get(&key) {
            bail!("{message}");
        }
        if let Some(script) = cache.get(&key).and_then(|entry| match entry {
            Cached::Ready(script) => script.upgrade(),
            Cached::Rejected(_) => None,
        }) {
            return Ok(script);
        }
        if cache
            .values()
            .filter(|entry| matches!(entry, Cached::Rejected(_)))
            .count()
            >= 128
        {
            cache.retain(
                |_, entry| matches!(entry, Cached::Ready(script) if script.strong_count() > 0),
            );
        }
        match result {
            Ok(script) => {
                cache.retain(|_, entry| match entry {
                    Cached::Ready(script) => script.strong_count() > 0,
                    Cached::Rejected(_) => true,
                });
                cache.insert(key, Cached::Ready(Arc::downgrade(&script)));
                Ok(script)
            }
            Err(error) => {
                // Endpoint changes must not repeatedly JIT the same rejected tenant plugin.
                cache.insert(
                    key,
                    Cached::Rejected(format!("{error:#}").chars().take(1024).collect()),
                );
                Err(error)
            }
        }
    }
    fn cached(&self, key: &[u8; 32]) -> Result<Option<Result<Arc<CompiledScript>>>> {
        let cache = self
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("compiler cache poisoned"))?;
        Ok(match cache.get(key) {
            Some(Cached::Ready(script)) => script.upgrade().map(Ok),
            Some(Cached::Rejected(message)) => Some(Err(anyhow::anyhow!("{message}"))),
            None => None,
        })
    }
    pub(crate) fn for_tenant(
        self: &Arc<Self>,
        namespace: &str,
        bytes: &[u8],
        per_minute: u32,
        wait: bool,
    ) -> Result<Arc<CompiledScript>> {
        ensure!(bytes.len() <= 1024 * 1024, "tenant script exceeds 1 MiB");
        let mut digest = Sha256::new();
        digest.update([0]);
        digest.update(bytes);
        let key = digest.finalize().into();
        if let Some(result) = self.cached(&key)? {
            return result;
        }
        let queue = self
            .queue
            .get_or_init(|| queue::Queue::start(Arc::downgrade(self)));
        queue.submit(key, namespace, bytes, per_minute)?;
        if wait {
            return queue.wait(self, &key);
        }
        bail!(PendingCompilation(
            "compilation pending; retaining accepted configuration while other updates proceed"
        ))
    }
    fn build(&self, bytes: &[u8], wasm: bool) -> Result<Arc<CompiledScript>> {
        let binary = if wasm {
            bytes.to_vec()
        } else {
            compile(std::str::from_utf8(bytes)?)?
        };
        budget::validate(&binary)?;
        let module = Module::from_binary(&self.engine, &binary)?;
        for import in module.imports() {
            ensure!(
                import.module() == "rgnix_v1"
                    && codegen::BUILTINS.iter().any(|b| b.name == import.name()),
                "unapproved Wasm import {}::{}",
                import.module(),
                import.name()
            );
        }
        let linker = make_linker(&self.engine)?;
        let script = Arc::new(CompiledScript {
            digest: format!("{:x}", Sha256::digest(&binary)),
            engine: self.engine.clone(),
            instance_pre: linker.instantiate_pre(&module)?,
        });
        let mut execution = script.instantiate(RequestData::default())?;
        execution
            .instance
            .get_typed_func::<(), i64>(&mut execution.store, "on_request")
            .context("plugin must export on_request() -> i64")?;
        if execution
            .instance
            .get_export(&mut execution.store, "on_response")
            .is_some()
        {
            execution
                .instance
                .get_typed_func::<(), i64>(&mut execution.store, "on_response")?;
        }
        Ok(script)
    }
}

fn read_input(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![];
    std::fs::File::open(path)
        .with_context(|| format!("read plugin {}", path.display()))?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "{} exceeds plugin input limit",
        path.display()
    );
    Ok(bytes)
}

pub fn read_source(path: &Path) -> Result<String> {
    Ok(String::from_utf8(read_input(path, 256 * 1024)?)?)
}

fn make_linker(engine: &Engine) -> Result<Linker<Host>> {
    let mut linker = Linker::new(engine);
    for builtin in codegen::BUILTINS {
        let name = builtin.name;
        let ty = wasmtime::FuncType::new(
            engine,
            vec![wasmtime::ValType::I64; builtin.args.len()],
            [wasmtime::ValType::I64],
        );
        linker.func_new("rgnix_v1", name, ty, move |mut caller, params, results| {
            let args: Vec<_> = params.iter().map(|v| v.i64().unwrap()).collect();
            results[0] = Val::I64(host_call(&mut caller, name, &args)?);
            Ok(())
        })?;
    }
    Ok(linker)
}

impl CompiledScript {
    fn instantiate(&self, request: RequestData) -> Result<Execution> {
        let initial = request.method.len()
            + request.path.len()
            + request.query.len()
            + request.host.len()
            + request.remote_addr.len()
            + request.body.as_ref().map_or(0, |body| body.bytes.len())
            + request
                .claims
                .iter()
                .map(|(k, v)| k.len() + v.len() + 64)
                .sum::<usize>()
            + request
                .headers
                .iter()
                .map(|(k, v)| k.len() + v.len() + 64)
                .sum::<usize>();
        ensure!(
            initial <= HOST_LIMIT,
            "request metadata exceeds plugin host limit"
        );
        let host = Host {
            request,
            outcome: RequestOutcome::default(),
            decision_set: false,
            response: false,
            status: 0,
            response_headers: BTreeMap::new(),
            response_edits: BTreeMap::new(),
            strings: vec![],
            allocated: initial,
            limits: StoreLimitsBuilder::new()
                .memory_size(MEMORY_LIMIT)
                .memories(1)
                .tables(0)
                .instances(1)
                .trap_on_grow_failure(true)
                .build(),
        };
        let mut store = Store::new(&self.engine, host);
        store.limiter(|host| &mut host.limits);
        store.set_fuel(FUEL)?;
        let instance = self.instance_pre.instantiate(&mut store)?;
        ensure!(
            instance.get_memory(&mut store, "memory").is_some(),
            "plugin must export memory"
        );
        // Instantiation may execute a start function. No start-time edits survive into a request.
        store.data_mut().outcome = RequestOutcome::default();
        store.data_mut().decision_set = false;
        Ok(Execution { store, instance })
    }
    pub fn request(&self, request: RequestData) -> Result<(Execution, RequestOutcome)> {
        let mut execution = self.instantiate(request)?;
        execution.store.set_fuel(FUEL)?;
        let func = execution
            .instance
            .get_typed_func::<(), i64>(&mut execution.store, "on_request")?;
        let action = func.call(&mut execution.store, ())?;
        ensure!((0..=2).contains(&action), "invalid route action");
        let outcome = execution.store.data().outcome.clone();
        let expected = match outcome.decision {
            Decision::Pass => 0,
            Decision::Proxy(_) => 1,
            Decision::Reply(..) => 2,
        };
        ensure!(
            action == expected,
            "returned route action does not match staged decision"
        );
        Ok((execution, outcome))
    }
}

impl Execution {
    pub fn response(
        &mut self,
        status: u16,
        headers: BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, Option<String>>> {
        if self
            .instance
            .get_export(&mut self.store, "on_response")
            .is_none()
        {
            return Ok(BTreeMap::new());
        }
        let extra = headers.iter().map(|(k, v)| k.len() + v.len() + 64).sum();
        self.store.data_mut().charge(extra)?;
        let host = self.store.data_mut();
        host.response = true;
        host.status = status;
        host.response_headers = headers;
        host.response_edits.clear();
        self.store.set_fuel(FUEL)?;
        self.instance
            .get_typed_func::<(), i64>(&mut self.store, "on_response")?
            .call(&mut self.store, ())?;
        Ok(std::mem::take(&mut self.store.data_mut().response_edits))
    }
}

impl Host {
    fn decide(&mut self, decision: Decision) -> Result<i64> {
        self.request_phase()?;
        // ABI v1 returns action kinds, so it cannot identify a superseded action value.
        ensure!(
            !self.decision_set,
            "only one routing decision is allowed per request hook"
        );
        let action = match &decision {
            Decision::Pass => 0,
            Decision::Proxy(_) => 1,
            Decision::Reply(..) => 2,
        };
        self.outcome.decision = decision;
        self.decision_set = true;
        Ok(action)
    }
    fn charge(&mut self, n: usize) -> Result<()> {
        self.allocated = self
            .allocated
            .checked_add(n)
            .context("host allocation overflow")?;
        ensure!(
            self.allocated <= HOST_LIMIT,
            "plugin host allocation limit exceeded"
        );
        Ok(())
    }
    fn string(&self, handle: i64) -> Result<&str> {
        ensure!(handle > 0, "nil string");
        self.strings
            .get((handle - 1) as usize)
            .map(String::as_str)
            .context("invalid string handle")
    }
    fn put(&mut self, string: String) -> Result<i64> {
        self.charge(string.len() + 32)?;
        self.strings.push(string);
        Ok(self.strings.len() as i64)
    }
    fn request_phase(&self) -> Result<()> {
        ensure!(
            !self.response,
            "request mutation is unavailable in on_response"
        );
        Ok(())
    }
    fn response_phase(&self) -> Result<()> {
        ensure!(
            self.response,
            "response headers are only available in on_response"
        );
        Ok(())
    }
}

fn host_call(caller: &mut Caller<'_, Host>, name: &str, args: &[i64]) -> Result<i64> {
    // Wasm instruction fuel does not account for host calls; charge every crossing as well.
    let fuel = caller.get_fuel()?;
    ensure!(fuel >= 100, "plugin host-call fuel exhausted");
    caller.set_fuel(fuel - 100)?;
    if name == "literal" {
        let ptr = usize::try_from(args[0])?;
        let len = usize::try_from(args[1])?;
        ensure!(len <= 65536, "literal exceeds 64 KiB");
        let end = ptr.checked_add(len).context("invalid literal range")?;
        let memory = caller
            .get_export("memory")
            .and_then(|e| e.into_memory())
            .context("missing memory")?;
        let text = std::str::from_utf8(
            memory
                .data(&*caller)
                .get(ptr..end)
                .context("literal out of bounds")?,
        )?
        .to_owned();
        return caller.data_mut().put(text);
    }
    if matches!(name, "str.hash" | "req.arg" | "req.cookie") {
        let bytes = match name {
            "str.hash" => caller.data().string(args[0])?.len(),
            "req.arg" => caller
                .data()
                .outcome
                .edits
                .query
                .as_ref()
                .unwrap_or(&caller.data().request.query)
                .len(),
            _ => {
                caller
                    .data()
                    .request
                    .headers
                    .get("cookie")
                    .map_or(0, String::len)
                    + caller
                        .data()
                        .outcome
                        .edits
                        .headers
                        .get("cookie")
                        .and_then(|v| v.as_ref())
                        .map_or(0, String::len)
            }
        };
        let cost = (bytes / 32) as u64;
        let fuel = caller.get_fuel()?;
        ensure!(fuel >= cost, "plugin parsing/hash fuel exhausted");
        caller.set_fuel(fuel - cost)?;
    }
    if matches!(
        name,
        "req.body" | "req.body_contains" | "req.json_string" | "req.json_int" | "req.json_bool"
    ) {
        let bytes = caller
            .data()
            .request
            .body
            .as_ref()
            .map_or(0, |body| body.bytes.len());
        let passes = if name.starts_with("req.json_") {
            let pointer = caller.data().string(args[0])?;
            ensure!(
                pointer.len() <= 1024 && pointer.bytes().filter(|b| *b == b'/').count() <= 32,
                "JSON pointer exceeds 1024 bytes or 32 components"
            );
            1 + pointer.bytes().filter(|b| *b == b'/').count()
        } else {
            1
        };
        let cost = (bytes.saturating_mul(passes) / 32) as u64;
        let fuel = caller.get_fuel()?;
        ensure!(fuel >= cost, "plugin body inspection fuel exhausted");
        caller.set_fuel(fuel - cost)?;
    }
    let host = caller.data_mut();
    match name {
        "req.body" => {
            let value = host
                .request
                .body
                .as_ref()
                .and_then(|b| std::str::from_utf8(&b.bytes).ok())
                .map(str::to_owned);
            value.map_or(Ok(0), |v| host.put(v))
        }
        "req.body_len" => Ok(host.request.body.as_ref().map_or(0, |b| b.bytes.len()) as i64),
        "req.body_complete" => Ok(i64::from(
            host.request.body.as_ref().is_some_and(|b| b.complete),
        )),
        "req.body_truncated" => Ok(i64::from(
            host.request.body.as_ref().is_some_and(|b| !b.complete),
        )),
        "req.body_contains" => {
            let needle = host.string(args[0])?;
            ensure!(needle.len() <= 8192, "body search pattern exceeds 8 KiB");
            Ok(i64::from(host.request.body.as_ref().is_some_and(|b| {
                memchr::memmem::find(&b.bytes, needle.as_bytes()).is_some()
            })))
        }
        "req.json_string" | "req.json_int" | "req.json_bool" => {
            let fallback = args.get(1).copied().unwrap_or(0);
            let Some(body) = &host.request.body else {
                return Ok(fallback);
            };
            if !body.complete {
                return Ok(fallback);
            }
            let bytes = body.bytes.clone();
            // Cover the decoder scratch space and current object key without retaining a DOM.
            host.charge(bytes.len() * 2 + 1024)?;
            let Some(value) = json::value_at(&bytes, host.string(args[0])?) else {
                return Ok(fallback);
            };
            match name {
                "req.json_int" => Ok(serde_json::from_str::<i64>(value.get()).unwrap_or(fallback)),
                "req.json_bool" => Ok(serde_json::from_str::<bool>(value.get())
                    .map(i64::from)
                    .unwrap_or(fallback)),
                _ => serde_json::from_str::<String>(value.get())
                    .ok()
                    .map_or(Ok(0), |v| host.put(v)),
            }
        }
        "req.arg" | "req.cookie" => {
            let key = host.string(args[0])?;
            ensure!(
                key.len() <= 256,
                "argument or cookie name exceeds 256 bytes"
            );
            let value = if name == "req.arg" {
                let query = host
                    .outcome
                    .edits
                    .query
                    .as_ref()
                    .unwrap_or(&host.request.query);
                url::form_urlencoded::parse(query.as_bytes())
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.into_owned())
            } else {
                let cookie = host
                    .outcome
                    .edits
                    .headers
                    .get("cookie")
                    .and_then(|v| v.as_deref())
                    .or_else(|| {
                        if host.outcome.edits.headers.contains_key("cookie") {
                            None
                        } else {
                            host.request.headers.get("cookie").map(String::as_str)
                        }
                    });
                cookie.and_then(|v| {
                    v.split(';')
                        .filter_map(|part| part.trim().split_once('='))
                        .find(|(k, _)| *k == key)
                        .map(|(_, v)| v.to_owned())
                })
            };
            value.map_or(Ok(0), |v| host.put(v))
        }
        "str.hash" => {
            let value = host.string(args[0])?;
            let hash = Sha256::digest(value.as_bytes());
            Ok((u64::from_be_bytes(hash[..8].try_into().unwrap()) & i64::MAX as u64) as i64)
        }
        "req.method" => host.put(host.request.method.clone()),
        "req.path" => host.put(
            host.outcome
                .edits
                .path
                .as_ref()
                .unwrap_or(&host.request.path)
                .clone(),
        ),
        "req.query" => host.put(
            host.outcome
                .edits
                .query
                .as_ref()
                .unwrap_or(&host.request.query)
                .clone(),
        ),
        "req.host" => host.put(host.request.host.clone()),
        "req.remote_addr" => host.put(host.request.remote_addr.clone()),
        "req.claim" => {
            let key = host.string(args[0])?;
            let value = host.request.claims.get(key).cloned();
            value.map_or(Ok(0), |v| host.put(v))
        }
        "req.header" | "resp.header" => {
            let key = host.string(args[0])?.to_ascii_lowercase();
            let value = if name == "req.header" {
                host.outcome
                    .edits
                    .headers
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| host.request.headers.get(&key).cloned())
            } else {
                host.response_phase()?;
                host.response_edits
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| host.response_headers.get(&key).cloned())
            };
            value.map_or(Ok(0), |v| host.put(v))
        }
        "req.set_path" | "req.set_query" => {
            host.request_phase()?;
            let value = host.string(args[0])?.to_owned();
            ensure!(
                value.len() <= 8192 && !value.contains(['\r', '\n', '#']),
                "invalid rewritten URI"
            );
            if name == "req.set_path" {
                ensure!(
                    value.starts_with('/') && !value.contains('?'),
                    "set_path requires an absolute path without query"
                );
            }
            host.charge(value.len())?;
            if name == "req.set_path" {
                host.outcome.edits.path = Some(value);
            } else {
                host.outcome.edits.query = Some(value);
            }
            Ok(0)
        }
        "req.set_header" | "req.remove_header" | "resp.set_header" | "resp.remove_header" => {
            let response = name.starts_with("resp.");
            if response {
                host.response_phase()?;
            } else {
                host.request_phase()?;
            }
            let key = host.string(args[0])?.to_ascii_lowercase();
            http::header::HeaderName::from_bytes(key.as_bytes())?;
            ensure!(
                ![
                    "connection",
                    "transfer-encoding",
                    "content-length",
                    "upgrade",
                    "trailer",
                    "te",
                    "keep-alive",
                    "proxy-connection"
                ]
                .contains(&key.as_str()),
                "plugin cannot modify transport header {key}"
            );
            let value = if name.ends_with("set_header") {
                let value = host.string(args[1])?.to_owned();
                ensure!(value.len() <= 8192, "header value exceeds 8 KiB");
                http::HeaderValue::from_str(&value)?;
                Some(value)
            } else {
                None
            };
            host.charge(key.len() + value.as_ref().map_or(0, String::len) + 64)?;
            if response {
                host.response_edits.insert(key, value);
            } else {
                host.outcome.edits.headers.insert(key, value);
            }
            Ok(0)
        }
        "resp.status" => {
            host.response_phase()?;
            Ok(i64::from(host.status))
        }
        "route.pass" => host.decide(Decision::Pass),
        "route.proxy" => {
            host.request_phase()?;
            let backend = host.string(args[0])?.to_owned();
            ensure!(backend.len() <= 512, "backend name too long");
            host.charge(backend.len())?;
            host.decide(Decision::Proxy(backend))
        }
        "resp.reply" => {
            host.request_phase()?;
            let status = u16::try_from(args[0])?;
            ensure!((200..=599).contains(&status), "invalid response status");
            let body = host.string(args[1])?.to_owned();
            ensure!(body.len() <= 65536, "reply exceeds 64 KiB");
            host.charge(body.len())?;
            host.decide(Decision::Reply(status, body))
        }
        "str.eq" => {
            if args[0] == 0 || args[1] == 0 {
                Ok(i64::from(args[0] == args[1]))
            } else {
                Ok(i64::from(host.string(args[0])? == host.string(args[1])?))
            }
        }
        "str.concat" => {
            let a = host.string(args[0])?;
            let b = host.string(args[1])?;
            ensure!(a.len() + b.len() <= 65536, "string exceeds 64 KiB");
            host.put(format!("{a}{b}"))
        }
        "str.starts_with" => Ok(i64::from(
            host.string(args[0])?.starts_with(host.string(args[1])?),
        )),
        "str.contains" => Ok(i64::from(
            host.string(args[0])?.contains(host.string(args[1])?),
        )),
        "str.len" => Ok(host.string(args[0])?.len() as i64),
        "str.lower" => {
            let value = host.string(args[0])?.to_ascii_lowercase();
            host.put(value)
        }
        _ => bail!("unknown host call {name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tenant_compilation_budget_does_not_block_cached_scripts_or_other_tenants() -> Result<()> {
        let compiler = Arc::new(Compiler::new()?);
        let first = b"function on_request() return route.pass() end";
        let second = b"function on_request() return resp.reply(200, \"other\") end";
        let script = compiler.for_tenant("one", first, 1, true)?;
        assert!(compiler.for_tenant("one", second, 1, true).is_err());
        assert!(Arc::ptr_eq(
            &script,
            &compiler.for_tenant("one", first, 1, true)?
        ));
        let other = compiler.for_tenant("two", second, 1, true)?;
        assert_ne!(script.digest, other.digest);
        assert!(
            compiler
                .for_tenant("three", b"invalid RGL", 1, true)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn body_access_is_bounded_and_json_requires_complete_input() -> Result<()> {
        use crate::body::BodyView;
        use bytes::Bytes;
        let compiler = Compiler::new()?;
        let script = compiler.from_bytes(
            br#"
function on_request()
    if req.json_string("/a~1b/~0key/1") == "vip" then return route.proxy("canary") end
    if req.body_contains("marker") and req.body() == nil then return resp.reply(201, "binary") end
    return route.pass()
end"#,
            false,
        )?;
        let input = br#"{"a/b":{"~key":["regular","vip"]}}"#;
        let request = |bytes: &[u8], complete| RequestData {
            body: Some(BodyView {
                bytes: Bytes::copy_from_slice(bytes),
                complete,
            }),
            ..RequestData::default()
        };
        assert!(matches!(
            script.request(request(input, true))?.1.decision,
            Decision::Proxy(_)
        ));
        assert!(matches!(
            script.request(request(input, false))?.1.decision,
            Decision::Pass
        ));
        assert!(matches!(
            script.request(request(b"marker\xff", false))?.1.decision,
            Decision::Reply(201, _)
        ));
        assert!(matches!(
            script.request(RequestData::default())?.1.decision,
            Decision::Pass
        ));
        for invalid in [
            br#"{"a/b":{"~key":["regular","vip"]}}garbage"#.as_slice(),
            br#"{"a/b":{"~key":["regular","vip"]},"a/b":null}"#,
        ] {
            assert!(matches!(
                script.request(request(invalid, true))?.1.decision,
                Decision::Pass
            ));
        }
        let expensive = compiler.from_bytes(b"function on_request() while true do local match = req.body_contains(\"absent\") end end", false)?;
        assert!(
            expensive
                .request(request(&vec![b'x'; crate::body::MAX_INSPECT_BYTES], false))
                .is_err()
        );
        let allocations = compiler.from_bytes(br#"function on_request() while true do local field = req.json_string("/tenant") end end"#, false)?;
        assert!(
            allocations
                .request(request(br#"{"tenant":"vip"}"#, true))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn compiled_functions_branches_and_response_hooks() -> Result<()> {
        let compiler = Compiler::for_runtime(1)?;
        let source = br#"
function choose(value)
    if value == "1" then return route.proxy("canary") end
    return route.pass()
end
function on_request()
    local n = 0
    while n < 3 do n = n + 1 end
    if n == 3 and req.header("absent") == nil then
        req.set_header("x-count", "three")
    end
    return choose(req.header("x-canary"))
end
function on_response()
    resp.set_header("x-result", str.lower("OK"))
end"#;
        let module = compiler.from_bytes(source, false)?;
        let mut request = RequestData::default();
        request.headers.insert("x-canary".into(), "1".into());
        let (mut execution, out) = module.request(request)?;
        assert!(matches!(out.decision,Decision::Proxy(ref name) if name=="canary"));
        assert_eq!(out.edits.headers["x-count"], Some("three".into()));
        assert_eq!(
            execution.response(200, BTreeMap::new())?["x-result"],
            Some("ok".into())
        );
        assert!(matches!(
            module.request(RequestData::default())?.1.decision,
            Decision::Pass
        ));
        for (first, second) in [
            ("route.proxy(\"app\")", "route.proxy(\"canary\")"),
            (
                "resp.reply(401, \"denied\")",
                "resp.reply(200, \"allowed\")",
            ),
        ] {
            let source = format!(
                "function on_request() local first = {first} local second = {second} return first end"
            );
            let script = compiler.from_bytes(source.as_bytes(), false)?;
            assert!(script.request(RequestData::default()).is_err());
        }
        Ok(())
    }
    #[test]
    fn compiler_rejects_recursion_and_runtime_bounds_loops() -> Result<()> {
        assert!(compile("function on_request() return on_request() end").is_err());
        assert!(compile("function on_request() local x = 1 x = true end").is_err());
        let compiler = Compiler::for_runtime(1)?;
        let script = compiler.from_bytes(b"function on_request() while true do end end", false)?;
        assert!(script.request(RequestData::default()).is_err());
        Ok(())
    }

    #[test]
    fn budgets_cover_host_allocations_and_imported_wasm_memory() -> Result<()> {
        let compiler = Compiler::for_runtime(1)?;
        let source = format!(
            "function on_request() local value = \"{}\" local n = 0 while n < 200 do req.set_header(\"x-budget\", value) n = n + 1 end return route.pass() end",
            "a".repeat(8192)
        );
        let script = compiler.from_bytes(source.as_bytes(), false)?;
        assert!(script.request(RequestData::default()).is_err());

        use wasm_encoder::{
            CodeSection, ExportKind, ExportSection, Function, FunctionSection, Instruction,
            MemorySection, MemoryType, Module, TypeSection, ValType,
        };
        let mut module = Module::new();
        let mut types = TypeSection::new();
        types.ty().function([], [ValType::I64]);
        module.section(&types);
        let mut functions = FunctionSection::new();
        functions.function(0);
        module.section(&functions);
        let mut memory = MemorySection::new();
        memory.memory(MemoryType {
            minimum: 129,
            maximum: Some(129),
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        module.section(&memory);
        let mut exports = ExportSection::new();
        exports.export("memory", ExportKind::Memory, 0);
        exports.export("on_request", ExportKind::Func, 0);
        module.section(&exports);
        let mut code = CodeSection::new();
        let mut function = Function::new([]);
        function
            .instruction(&Instruction::I64Const(0))
            .instruction(&Instruction::End);
        code.function(&function);
        module.section(&code);
        assert!(compiler.from_bytes(&module.finish(), true).is_err());
        Ok(())
    }

    #[test]
    fn pooled_instances_reset_guest_state_and_leave_control_plane_capacity() -> Result<()> {
        use wasm_encoder::{
            BlockType, CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, Function,
            FunctionSection, GlobalSection, GlobalType, Instruction as I, MemArg, MemorySection,
            MemoryType, Module, TypeSection, ValType,
        };
        let mut module = Module::new();
        let mut types = TypeSection::new();
        types.ty().function([], [ValType::I64]);
        module.section(&types);
        let mut functions = FunctionSection::new();
        functions.function(0);
        functions.function(0);
        module.section(&functions);
        let mut memory = MemorySection::new();
        memory.memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        module.section(&memory);
        let mut globals = GlobalSection::new();
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(7),
        );
        module.section(&globals);
        let mut exports = ExportSection::new();
        exports.export("memory", ExportKind::Memory, 0);
        exports.export("on_request", ExportKind::Func, 0);
        exports.export("on_response", ExportKind::Func, 1);
        module.section(&exports);
        let address = MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        };
        let mut code = CodeSection::new();
        for response in [false, true] {
            let mut function = Function::new([]);
            for instruction in [
                I::I32Const(0),
                I::I32Load8U(address),
                I::I32Const(if response { 99 } else { 7 }),
                I::I32Ne,
                I::If(BlockType::Empty),
                I::Unreachable,
                I::End,
                I::GlobalGet(0),
                I::I32Const(if response { 99 } else { 7 }),
                I::I32Ne,
                I::If(BlockType::Empty),
                I::Unreachable,
                I::End,
                I::MemorySize(0),
                I::I32Const(if response { 2 } else { 1 }),
                I::I32Ne,
                I::If(BlockType::Empty),
                I::Unreachable,
                I::End,
            ] {
                function.instruction(&instruction);
            }
            if !response {
                for instruction in [
                    I::I32Const(0),
                    I::I32Const(99),
                    I::I32Store8(address),
                    I::I32Const(99),
                    I::GlobalSet(0),
                    I::I32Const(1),
                    I::MemoryGrow(0),
                    I::Drop,
                ] {
                    function.instruction(&instruction);
                }
            }
            function.instruction(&I::I64Const(0)).instruction(&I::End);
            code.function(&function);
        }
        module.section(&code);
        let mut data = DataSection::new();
        data.active(0, &ConstExpr::i32_const(0), [7]);
        module.section(&data);

        let compiler = Compiler::for_runtime(1)?;
        let script = compiler.from_bytes(&module.finish(), true)?;
        let mut held = Vec::new();
        // One HTTP execution and two simulations fill their respective budgets.
        for _ in 0..3 {
            held.push(script.request(RequestData::default())?.0);
        }
        let other = compiler.from_bytes(b"function on_request() return route.pass() end", false)?;
        held.push(script.request(RequestData::default())?.0);
        assert!(script.request(RequestData::default()).is_err());
        for mut execution in held {
            execution.response(200, BTreeMap::new())?;
        }
        for _ in 0..20 {
            let (mut execution, _) = script.request(RequestData::default())?;
            execution.response(200, BTreeMap::new())?;
            drop(execution);
            other.request(RequestData::default())?;
        }
        // Trap cleanup must also make the slot safe to reuse by another module.
        let trap = compiler.from_bytes(b"function on_request() while true do end end", false)?;
        for _ in 0..8 {
            assert!(trap.request(RequestData::default()).is_err());
            script.request(RequestData::default())?;
        }
        Ok(())
    }
}
