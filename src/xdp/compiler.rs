use super::config::{self, Metadata, Rule};
use crate::script::syntax::{self, Expr, Stmt};
use anyhow::{Context, Result, bail, ensure};
use ipnet::IpNet;
use std::{
    collections::BTreeMap,
    io::Read,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq)]
enum Type {
    Int,
    Bool,
}

struct Builder {
    next_local: usize,
    rate_slots: usize,
    nodes: usize,
    metadata: Metadata,
    locations: Vec<(usize, usize)>,
    statement: usize,
    location: (usize, usize),
}

/// Translate the bounded packet subset of RGL to C for Clang's eBPF backend.
/// User strings are parsed as CIDRs; none are interpolated as C identifiers or code.
fn translate(source: &str, dispatcher: Option<&config::Config>) -> Result<String> {
    ensure!(source.len() <= 64 * 1024, "XDP source exceeds 64 KiB");
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("rgl-xdp-compiler".into())
            .stack_size(8 * 1024 * 1024)
            .spawn_scoped(scope, || {
                let syntax::Parsed { functions: ast, locations } = syntax::parse_located(source, "on_xdp")?;
                ensure!(
                    ast.len() == 1 && ast[0].params.is_empty(),
                    "XDP requires only function on_xdp() with no parameters; helper functions are unsupported"
                );
                let mut builder = Builder {
                    next_local: 0,
                    rate_slots: 0,
                    nodes: 0,
                    metadata: Metadata { abi: config::ABI, compiler: env!("CARGO_PKG_VERSION").into(), source_sha256: config::digest(source.as_bytes()), rules: vec![], sets: vec![], keyed_rates: false, rate_ids: vec![], dispatcher_config: dispatcher.cloned() },
                    locations, statement: 0, location: (1, 1),
                };
                let body = builder.block(&ast[0].body, &BTreeMap::new(), 0)?;
                let defaults = if let Some(config) = dispatcher {
                    ensure!(builder.metadata.sets.is_empty() && config.sets.is_empty(), "dispatcher artifacts use immutable literal CIDRs; dynamic sets require the managed agent");
                    ensure!(config.event_sample_every == 0, "dispatcher artifacts have no managed OTLP consumer");
                    let destinations = config.scope.destinations.iter().map(|cidr| builder.call("pkt.dst_in", &[Expr::Str(cidr.to_string())]).map(|v| v.0)).collect::<Result<Vec<_>>>()?;
                    let destinations = if destinations.is_empty() { "1".into() } else { destinations.join(" || ") };
                    let ports = config.scope.ports.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
                    let protocols = config.scope.protocols.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
                    let action = |v| match v { config::Action::Pass => 2, config::Action::Drop => 1, config::Action::Policy => 0 };
                    format!("#define DISPATCHER 1\nstatic const struct settings dispatcher_settings = {{.observe={},.malformed={},.unsupported={},.fragments={},.port_count={},.protocol_count={},.ceiling_rate={},.ceiling_burst={},.ceiling_id={}ULL,.ports={{{ports}}},.protocols={{{protocols}}}}};\nstatic __always_inline int dispatcher_destination(struct packet *p) {{ return {destinations}; }}\n", u8::from(config.observe), action(config.malformed), action(config.unsupported), action(config.fragments), config.scope.ports.len(), config.scope.protocols.len(), config.ceiling_pps, config.ceiling_burst, config::id(format!("ceiling:{}:{}", config.ceiling_pps, config.ceiling_burst).as_bytes()))
                } else { String::new() };
                let metadata = serde_json::to_vec(&builder.metadata)?;
                let metadata = metadata.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
                Ok(format!(
                    "{}\n{}\n{defaults}\n#define KEYED_RATES {}\nstatic __always_inline int policy(struct packet *p) {{\n{body}return 2;\n}}\n{}\nconst u8 SEC(\"rgnix_meta\") metadata[] = {{{metadata}}};\n",
                    include_str!("packet.c"), include_str!("policy.c"), u8::from(builder.metadata.keyed_rates), include_str!("entry.c")
                ))
            })?
            .join()
            .map_err(|_| anyhow::anyhow!("XDP compiler panicked"))?
    })
}

impl Builder {
    fn count(&mut self, depth: usize) -> Result<()> {
        self.nodes += 1;
        ensure!(
            self.nodes <= 1024 && depth <= 32,
            "XDP policy exceeds 1024 nodes or nesting depth 32"
        );
        Ok(())
    }
    fn block(
        &mut self,
        statements: &[Stmt],
        parent: &BTreeMap<String, (String, Type)>,
        depth: usize,
    ) -> Result<String> {
        let mut locals = parent.clone();
        let mut code = String::new();
        for statement in statements {
            self.location = self.locations[self.statement];
            self.statement += 1;
            self.count(depth)?;
            let location = self.location;
            code.push_str(&format!("\n#line {} \"policy.rgl\"\n", location.0));
            let generated = (|| -> Result<()> {
                match statement {
                    Stmt::Local(name, expr) => {
                        let (value, ty) = self.expr(expr, &locals, depth + 1)?;
                        let var = format!("v{}", self.next_local);
                        self.next_local += 1;
                        ensure!(self.next_local <= 64, "XDP supports at most 64 locals");
                        code.push_str(&format!("__attribute__((unused)) u64 {var} = {value};\n"));
                        locals.insert(name.clone(), (var, ty));
                    }
                    Stmt::Assign(name, expr) => {
                        let (var, ty) = locals
                            .get(name)
                            .with_context(|| format!("unknown local {name}"))?
                            .clone();
                        let (value, value_ty) = self.expr(expr, &locals, depth + 1)?;
                        ensure!(ty == value_ty, "type mismatch assigning {name}");
                        code.push_str(&format!("{var} = {value};\n"));
                    }
                    Stmt::Return(Some(Expr::Call(name, args)))
                        if ["xdp.pass", "xdp.drop"].contains(&name.as_str()) =>
                    {
                        let label = match args.as_slice() {
                            [] => format!(
                                "{}-{}-{}",
                                if name == "xdp.pass" { "pass" } else { "drop" },
                                location.0,
                                location.1
                            ),
                            [Expr::Str(label)] => label.clone(),
                            _ => bail!("verdict accepts an optional literal rule name"),
                        };
                        config::valid_name(&label)?;
                        ensure!(
                            ![
                                "default",
                                "scope_bypass",
                                "malformed",
                                "unsupported",
                                "fragment",
                                "global_ceiling",
                                "reserved6",
                                "reserved7"
                            ]
                            .contains(&label.as_str()),
                            "reserved system rule name {label}"
                        );
                        ensure!(
                            !self.metadata.rules.iter().any(|r| r.name == label),
                            "duplicate rule name {label}"
                        );
                        ensure!(
                            self.metadata.rules.len() < config::MAX_RULES,
                            "at most 128 named verdicts"
                        );
                        let slot = self.metadata.rules.len() + 8;
                        self.metadata.rules.push(Rule {
                            name: label,
                            line: location.0,
                            column: location.1,
                        });
                        code.push_str(&format!(
                            "return {};\n",
                            slot * 4 + if name == "xdp.pass" { 2 } else { 1 }
                        ));
                    }
                    Stmt::If(branches, other) => {
                        for (index, (condition, body)) in branches.iter().enumerate() {
                            let (value, ty) = self.expr(condition, &locals, depth + 1)?;
                            ensure!(ty == Type::Bool, "XDP if condition must be boolean");
                            code.push_str(&format!(
                                "{}if ({value} != 0) {{\n{} }}",
                                if index == 0 { "" } else { " else " },
                                self.block(body, &locals, depth + 1)?
                            ));
                        }
                        if !other.is_empty() {
                            code.push_str(&format!(
                                " else {{\n{} }}",
                                self.block(other, &locals, depth + 1)?
                            ));
                        }
                        code.push('\n');
                    }
                    _ => bail!(
                        "XDP supports local, assignment, if/elseif/else and return xdp.pass()/xdp.drop(); loops, HTTP APIs and other statements are unsupported"
                    ),
                }
                Ok(())
            })();
            generated.with_context(|| format!("{}:{}", location.0, location.1))?;
        }
        Ok(code)
    }
    fn expr(
        &mut self,
        expr: &Expr,
        locals: &BTreeMap<String, (String, Type)>,
        depth: usize,
    ) -> Result<(String, Type)> {
        self.count(depth)?;
        match expr {
            Expr::Int(n) => {
                ensure!(
                    (0..=u32::MAX as i64).contains(n),
                    "XDP integer must be 0..4294967295"
                );
                Ok((format!("{n}ULL"), Type::Int))
            }
            Expr::Bool(v) => Ok((u8::from(*v).to_string(), Type::Bool)),
            Expr::Var(name) => locals
                .get(name)
                .cloned()
                .with_context(|| format!("unknown XDP local {name}")),
            Expr::Unary(op, value) if op == "not" => {
                let (value, ty) = self.expr(value, locals, depth + 1)?;
                ensure!(ty == Type::Bool, "not requires a boolean");
                Ok((format!("(!({value}))"), Type::Bool))
            }
            Expr::Binary(op, left, right) => {
                let (left, lt) = self.expr(left, locals, depth + 1)?;
                let (right, rt) = self.expr(right, locals, depth + 1)?;
                let operator = match op.as_str() {
                    "and" | "or" => {
                        ensure!(
                            lt == Type::Bool && rt == Type::Bool,
                            "and/or require booleans"
                        );
                        if op == "and" { "&&" } else { "||" }
                    }
                    "==" | "~=" => {
                        ensure!(lt == rt, "comparison type mismatch");
                        if op == "==" { "==" } else { "!=" }
                    }
                    "<" | "<=" | ">" | ">=" => {
                        ensure!(
                            lt == Type::Int && rt == Type::Int,
                            "ordered comparison requires integers"
                        );
                        op.as_str()
                    }
                    _ => bail!("unsupported XDP operator {op}"),
                };
                Ok((format!("(({left}) {operator} ({right}))"), Type::Bool))
            }
            Expr::Call(name, args) => self.call(name, args),
            _ => bail!(
                "unsupported XDP expression; strings are only allowed as literal CIDR arguments"
            ),
        }
    }
    fn call(&mut self, name: &str, args: &[Expr]) -> Result<(String, Type)> {
        if ["pkt.src_in", "pkt.dst_in"].contains(&name) {
            let [Expr::Str(cidr)] = args else {
                bail!("{name} requires one literal IPv4/IPv6 CIDR")
            };
            let network: IpNet = cidr
                .parse()
                .with_context(|| format!("invalid CIDR {cidr}"))?;
            let field = if name == "pkt.src_in" { "src" } else { "dst" };
            let (version, bytes, mut bits) = match network {
                IpNet::V4(net) => (4, net.network().octets().to_vec(), net.prefix_len()),
                IpNet::V6(net) => (6, net.network().octets().to_vec(), net.prefix_len()),
            };
            let mut predicates = vec![format!("p->version == {version}")];
            for (i, byte) in bytes.iter().enumerate() {
                if bits == 0 {
                    break;
                }
                let take = bits.min(8);
                let mask = 255u8 << (8 - take);
                predicates.push(format!("(p->{field}[{i}] & {mask}) == {}", byte & mask));
                bits -= take;
            }
            return Ok((format!("({})", predicates.join(" && ")), Type::Bool));
        }
        if ["pkt.src_in_set", "pkt.dst_in_set"].contains(&name) {
            let [Expr::Str(set)] = args else {
                bail!("{name} requires a literal set name");
            };
            config::valid_name(set)?;
            let slot = if let Some(slot) = self.metadata.sets.iter().position(|s| s == set) {
                slot
            } else {
                ensure!(
                    self.metadata.sets.len() < config::MAX_SETS,
                    "at most 32 address sets"
                );
                self.metadata.sets.push(set.clone());
                self.metadata.sets.len() - 1
            };
            return Ok((
                format!("in_set(p, {slot}, {})", u8::from(name == "pkt.dst_in_set")),
                Type::Bool,
            ));
        }
        if ["xdp.allow", "xdp.allow_bytes"].contains(&name) {
            let [
                Expr::Str(label),
                Expr::Str(key),
                Expr::Int(rate),
                Expr::Int(burst),
            ] = args
            else {
                bail!("{name} requires (literal name, key, rate, burst)");
            };
            config::valid_name(label)?;
            ensure!(
                (1..=1_000_000_000).contains(rate) && (1..=1_000_000_000).contains(burst),
                "rate/burst must be 1..1000000000"
            );
            let (kind, prefix4, prefix6) = match key.as_str() {
                "global" => (0, 0, 0),
                "src_ip" => (1, 32, 128),
                "src_subnet" => (1, 24, 64),
                "dst_port" => (2, 0, 0),
                "src_ip_dst_port" => (3, 32, 128),
                _ => bail!("key must be global, src_ip, src_subnet, dst_port or src_ip_dst_port"),
            };
            self.metadata.keyed_rates |= kind != 0;
            let id = config::id(format!("{name}:{label}:{key}:{rate}:{burst}").as_bytes());
            ensure!(
                self.metadata.rate_ids.len() < 64,
                "at most 64 named limiters"
            );
            ensure!(
                !self.metadata.rate_ids.contains(&id),
                "duplicate limiter definition {label}"
            );
            self.metadata.rate_ids.push(id);
            return Ok((
                format!(
                    "token_rate(p, {id}ULL, {kind}, {prefix4}, {prefix6}, {rate}, {burst}, {}, 1)",
                    if name == "xdp.allow_bytes" {
                        "p->length"
                    } else {
                        "1"
                    }
                ),
                Type::Bool,
            ));
        }
        if name == "xdp.allow_rate" {
            let [Expr::Int(limit)] = args else {
                bail!("xdp.allow_rate requires a literal packets-per-second limit")
            };
            ensure!(
                (1..=1_000_000_000).contains(limit),
                "packet rate must be 1..1000000000"
            );
            ensure!(
                self.rate_slots < 64,
                "XDP supports at most 64 rate limit call sites"
            );
            let code = format!("allow_rate({}, {limit})", self.rate_slots);
            self.rate_slots += 1;
            return Ok((code, Type::Bool));
        }
        ensure!(args.is_empty(), "{name} takes no arguments");
        let (code, ty) = match name {
            "pkt.ip_version" => ("p->version", Type::Int),
            "pkt.protocol" => ("p->protocol", Type::Int),
            "pkt.src_port" => ("p->src_port", Type::Int),
            "pkt.dst_port" => ("p->dst_port", Type::Int),
            "pkt.len" => ("p->length", Type::Int),
            "pkt.is_tcp" => ("p->protocol == 6", Type::Bool),
            "pkt.is_udp" => ("p->protocol == 17", Type::Bool),
            "pkt.tcp_syn" => (
                "p->protocol == 6 && p->ports && (p->tcp_flags & 0x12) == 2",
                Type::Bool,
            ),
            "pkt.has_ports" => ("p->ports", Type::Bool),
            "pkt.fragmented" => ("p->fragmented", Type::Bool),
            _ => bail!(
                "unsupported XDP API {name}; HTTP req/resp/route APIs are unavailable in on_xdp"
            ),
        };
        Ok((format!("({code})"), ty))
    }
}

pub fn compile(source: &str, clang: &Path) -> Result<Vec<u8>> {
    compile_mode(source, clang, None)
}
pub fn compile_mode(
    source: &str,
    clang: &Path,
    dispatcher: Option<&config::Config>,
) -> Result<Vec<u8>> {
    let code = translate(source, dispatcher)?;
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("policy.c");
    let output = directory.path().join("policy.o");
    let errors = directory.path().join("errors");
    std::fs::write(&input, code)?;
    let mut child = std::process::Command::new(clang)
        .args([
            "-target",
            "bpfel",
            "-mcpu=v3",
            "-O2",
            "-g",
            "-fdebug-compilation-dir=.",
            "-Wall",
            "-Werror",
            "-Wno-unused-function",
            "-fno-stack-protector",
            "-c",
        ])
        .current_dir(directory.path())
        .arg("policy.c")
        .arg("-o")
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&errors)?)
        .spawn()
        .context("start Clang with eBPF support; install clang or use a precompiled .o")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("XDP Clang compilation exceeded 30 seconds");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut diagnostics = String::new();
    std::fs::File::open(errors)?
        .take(16 * 1024)
        .read_to_string(&mut diagnostics)?;
    ensure!(
        status.success(),
        "XDP Clang compilation failed: {diagnostics}"
    );
    super::read_object(&output)
}
