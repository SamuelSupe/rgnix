mod budget;
mod codegen;
mod syntax;
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    io::Read,
    path::Path,
    sync::{Arc, Mutex, Weak},
};
use wasmtime::{
    Caller, Config, Engine, Instance, InstancePre, Linker, Module, Store, StoreLimits,
    StoreLimitsBuilder, Strategy, Val,
};

pub use codegen::compile;
const HOST_LIMIT: usize = 1024 * 1024;
const FUEL: u64 = 100_000;

pub struct Compiler {
    engine: Engine,
    cache: Mutex<HashMap<[u8; 32], Cached>>,
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
    pub method: String,
    pub path: String,
    pub query: String,
    pub host: String,
    pub remote_addr: String,
    pub headers: BTreeMap<String, String>,
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
        let mut config = Config::new();
        config
            .strategy(Strategy::Cranelift)
            .consume_fuel(true)
            .max_wasm_stack(256 * 1024);
        Ok(Self {
            engine: Engine::new(&config)?,
            cache: Mutex::new(HashMap::new()),
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
        match self.build(bytes, wasm) {
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
                .memory_size(8 * 1024 * 1024)
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
    let host = caller.data_mut();
    match name {
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
    fn compiled_functions_branches_and_response_hooks() -> Result<()> {
        let compiler = Compiler::new()?;
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
        let compiler = Compiler::new()?;
        let script = compiler.from_bytes(b"function on_request() while true do end end", false)?;
        assert!(script.request(RequestData::default()).is_err());
        Ok(())
    }

    #[test]
    fn budgets_cover_host_allocations_and_imported_wasm_memory() -> Result<()> {
        let compiler = Compiler::new()?;
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
}
