use super::syntax::{self, Expr, Stmt};
use anyhow::{Context, Result, bail, ensure};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};
use wasm_encoder::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    Int,
    Bool,
    String,
    Nil,
    Action,
    Void,
}
pub struct Builtin {
    pub name: &'static str,
    pub args: &'static [Type],
    pub result: Type,
}
use Type::*;
pub const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "literal",
        args: &[Int, Int],
        result: String,
    },
    Builtin {
        name: "req.method",
        args: &[],
        result: String,
    },
    Builtin {
        name: "req.path",
        args: &[],
        result: String,
    },
    Builtin {
        name: "req.query",
        args: &[],
        result: String,
    },
    Builtin {
        name: "req.host",
        args: &[],
        result: String,
    },
    Builtin {
        name: "req.remote_addr",
        args: &[],
        result: String,
    },
    Builtin {
        name: "req.header",
        args: &[String],
        result: String,
    },
    Builtin {
        name: "req.set_path",
        args: &[String],
        result: Void,
    },
    Builtin {
        name: "req.set_query",
        args: &[String],
        result: Void,
    },
    Builtin {
        name: "req.set_header",
        args: &[String, String],
        result: Void,
    },
    Builtin {
        name: "req.remove_header",
        args: &[String],
        result: Void,
    },
    Builtin {
        name: "resp.status",
        args: &[],
        result: Int,
    },
    Builtin {
        name: "resp.header",
        args: &[String],
        result: String,
    },
    Builtin {
        name: "resp.set_header",
        args: &[String, String],
        result: Void,
    },
    Builtin {
        name: "resp.remove_header",
        args: &[String],
        result: Void,
    },
    Builtin {
        name: "route.pass",
        args: &[],
        result: Action,
    },
    Builtin {
        name: "route.proxy",
        args: &[String],
        result: Action,
    },
    Builtin {
        name: "resp.reply",
        args: &[Int, String],
        result: Action,
    },
    Builtin {
        name: "str.concat",
        args: &[String, String],
        result: String,
    },
    Builtin {
        name: "str.eq",
        args: &[String, String],
        result: Bool,
    },
    Builtin {
        name: "str.starts_with",
        args: &[String, String],
        result: Bool,
    },
    Builtin {
        name: "str.contains",
        args: &[String, String],
        result: Bool,
    },
    Builtin {
        name: "str.len",
        args: &[String],
        result: Int,
    },
    Builtin {
        name: "str.lower",
        args: &[String],
        result: String,
    },
];

pub fn compile(source: &str) -> Result<Vec<u8>> {
    // CLI and control-plane workers must have the same bounded compiler stack.
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("rgl-compiler".into())
            .stack_size(8 * 1024 * 1024)
            .spawn_scoped(scope, || compile_inner(source))?
            .join()
            .map_err(|_| anyhow::anyhow!("RGL compiler panicked"))?
    })
}

fn compile_inner(source: &str) -> Result<Vec<u8>> {
    let ast = syntax::parse(source)?;
    let mut builder = Builder {
        ast: ast.into_iter().map(|f| (f.name.clone(), f)).collect(),
        functions: vec![],
        signatures: BTreeMap::new(),
        active: BTreeSet::new(),
        data: vec![],
        depth: 0,
    };
    let (request, ty) = builder.function("on_request", &[])?;
    ensure!(
        matches!(ty, Action | Void),
        "on_request must return a route action or nothing"
    );
    let response = if builder.ast.contains_key("on_response") {
        let (id, ty) = builder.function("on_response", &[])?;
        ensure!(ty == Void, "on_response cannot return a value");
        Some(id)
    } else {
        None
    };
    let mut module = Module::new();
    module.section(&CustomSection {
        name: Cow::Borrowed("rgnix.abi"),
        data: Cow::Borrowed(b"1"),
    });
    let mut types = TypeSection::new();
    let mut imports = ImportSection::new();
    for (i, b) in BUILTINS.iter().enumerate() {
        types
            .ty()
            .function(vec![ValType::I64; b.args.len()], [ValType::I64]);
        imports.import("rgnix_v1", b.name, EntityType::Function(i as u32));
    }
    let mut functions = FunctionSection::new();
    let mut code = CodeSection::new();
    for (i, f) in builder.functions.into_iter().enumerate() {
        let f = f.context("incomplete function")?;
        types
            .ty()
            .function(vec![ValType::I64; f.params], [ValType::I64]);
        functions.function((BUILTINS.len() + i) as u32);
        let mut body = wasm_encoder::Function::new([(f.locals, ValType::I64)]);
        for instruction in &f.code {
            body.instruction(instruction);
        }
        code.function(&body);
    }
    module.section(&types);
    module.section(&imports);
    module.section(&functions);
    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: (builder.data.len() as u64).div_ceil(65536).max(1),
        maximum: Some(128),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&memory);
    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("on_request", ExportKind::Func, request);
    if let Some(id) = response {
        exports.export("on_response", ExportKind::Func, id);
    }
    module.section(&exports);
    module.section(&code);
    let mut data = DataSection::new();
    data.active(0, &ConstExpr::i32_const(0), builder.data);
    module.section(&data);
    Ok(module.finish())
}

struct BuiltFunction {
    params: usize,
    locals: u32,
    code: Vec<Instruction<'static>>,
}
struct Builder {
    ast: BTreeMap<std::string::String, syntax::Function>,
    functions: Vec<Option<BuiltFunction>>,
    signatures: BTreeMap<std::string::String, (u32, Vec<Type>, Type)>,
    active: BTreeSet<std::string::String>,
    data: Vec<u8>,
    depth: usize,
}
impl Builder {
    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        ensure!(
            self.depth <= 128,
            "combined compiler traversal depth exceeds 128"
        );
        Ok(())
    }
    fn function(&mut self, name: &str, args: &[Type]) -> Result<(u32, Type)> {
        self.enter()?;
        let result = self.function_inner(name, args);
        self.depth -= 1;
        result
    }
    fn function_inner(&mut self, name: &str, args: &[Type]) -> Result<(u32, Type)> {
        ensure!(
            !self.active.contains(name),
            "recursive call to {name} is unsupported"
        );
        if let Some((id, known, ty)) = self.signatures.get(name) {
            ensure!(
                known == args,
                "function {name} called with inconsistent argument types"
            );
            return Ok((*id, *ty));
        }
        let ast = self
            .ast
            .get(name)
            .with_context(|| format!("unknown function {name}"))?
            .clone();
        ensure!(
            ast.params.len() == args.len(),
            "wrong argument count for {name}"
        );
        ensure!(self.active.len() < 64, "call depth exceeds 64");
        self.active.insert(name.into());
        let slot = self.functions.len();
        let id = (BUILTINS.len() + slot) as u32;
        self.functions.push(None);
        let locals = ast
            .params
            .iter()
            .zip(args)
            .enumerate()
            .map(|(i, (n, t))| (n.clone(), (i as u32, *t)))
            .collect();
        let mut f = FunctionCompiler {
            builder: self,
            vars: locals,
            next: args.len() as u32,
            code: vec![],
            result: None,
        };
        f.block(&ast.body)?;
        let ty = f.result.unwrap_or(Void);
        ensure!(
            name == "on_request" || ty == Void || returns(&ast.body),
            "function {name} must return on every path"
        );
        f.code.extend([Instruction::I64Const(0), Instruction::End]);
        let built = BuiltFunction {
            params: args.len(),
            locals: f.next - args.len() as u32,
            code: f.code,
        };
        self.functions[slot] = Some(built);
        self.active.remove(name);
        self.signatures.insert(name.into(), (id, args.to_vec(), ty));
        Ok((id, ty))
    }
}
fn compatible(expected: Type, actual: Type) -> bool {
    expected == actual || expected == String && actual == Nil
}
fn returns(body: &[Stmt]) -> bool {
    body.last().is_some_and(|s| match s {
        Stmt::Return(_) => true,
        Stmt::If(branches, other) => returns(other) && branches.iter().all(|(_, b)| returns(b)),
        _ => false,
    })
}

struct FunctionCompiler<'a> {
    builder: &'a mut Builder,
    vars: BTreeMap<std::string::String, (u32, Type)>,
    next: u32,
    code: Vec<Instruction<'static>>,
    result: Option<Type>,
}
impl FunctionCompiler<'_> {
    fn block(&mut self, body: &[Stmt]) -> Result<()> {
        self.builder.enter()?;
        let result = self.block_inner(body);
        self.builder.depth -= 1;
        result
    }
    fn block_inner(&mut self, body: &[Stmt]) -> Result<()> {
        let outer = self.vars.clone();
        for s in body {
            self.statement(s)?;
        }
        self.vars = outer;
        Ok(())
    }
    fn statement(&mut self, s: &Stmt) -> Result<()> {
        match s {
            Stmt::Local(name, e) => {
                let ty = self.expr(e)?;
                ensure!(
                    !matches!(ty, Void | Nil),
                    "local {name} needs a concrete non-void type"
                );
                let id = self.next;
                self.next += 1;
                self.vars.insert(name.clone(), (id, ty));
                self.code.push(Instruction::LocalSet(id));
            }
            Stmt::Assign(name, e) => {
                let (id, ty) = *self
                    .vars
                    .get(name)
                    .with_context(|| format!("unknown variable {name}"))?;
                let actual = self.expr(e)?;
                ensure!(compatible(ty, actual), "assignment changes type of {name}");
                self.code.push(Instruction::LocalSet(id));
            }
            Stmt::Call(e) => {
                self.expr(e)?;
                self.code.push(Instruction::Drop);
            }
            Stmt::Return(e) => {
                let ty = if let Some(e) = e {
                    self.expr(e)?
                } else {
                    self.code.push(Instruction::I64Const(0));
                    Void
                };
                if let Some(expected) = self.result {
                    ensure!(expected == ty, "inconsistent function return types");
                }
                self.result = Some(ty);
                self.code.push(Instruction::Return);
            }
            Stmt::While(e, body) => {
                self.code.extend([
                    Instruction::Block(BlockType::Empty),
                    Instruction::Loop(BlockType::Empty),
                ]);
                ensure!(self.expr(e)? == Bool, "while condition must be boolean");
                self.code
                    .extend([Instruction::I64Eqz, Instruction::BrIf(1)]);
                self.block(body)?;
                self.code
                    .extend([Instruction::Br(0), Instruction::End, Instruction::End]);
            }
            Stmt::If(branches, other) => {
                self.branch(branches, other)?;
            }
        }
        Ok(())
    }
    fn branch(&mut self, branches: &[(Expr, Vec<Stmt>)], other: &[Stmt]) -> Result<()> {
        self.builder.enter()?;
        let result = self.branch_inner(branches, other);
        self.builder.depth -= 1;
        result
    }
    fn branch_inner(&mut self, branches: &[(Expr, Vec<Stmt>)], other: &[Stmt]) -> Result<()> {
        if let Some(((condition, body), rest)) = branches.split_first() {
            ensure!(
                self.expr(condition)? == Bool,
                "if condition must be boolean"
            );
            self.code.extend([
                Instruction::I64Eqz,
                Instruction::I32Eqz,
                Instruction::If(BlockType::Empty),
            ]);
            self.block(body)?;
            self.code.push(Instruction::Else);
            self.branch(rest, other)?;
            self.code.push(Instruction::End);
        } else {
            self.block(other)?;
        }
        Ok(())
    }
    fn call_builtin(&mut self, name: &str) -> Result<Type> {
        let (id, b) = BUILTINS
            .iter()
            .enumerate()
            .find(|(_, b)| b.name == name)
            .context("missing compiler builtin")?;
        self.code.push(Instruction::Call(id as u32));
        Ok(b.result)
    }
    fn expr(&mut self, e: &Expr) -> Result<Type> {
        self.builder.enter()?;
        let result = self.expr_inner(e);
        self.builder.depth -= 1;
        result
    }
    fn expr_inner(&mut self, e: &Expr) -> Result<Type> {
        match e {
            Expr::Int(n) => {
                self.code.push(Instruction::I64Const(*n));
                Ok(Int)
            }
            Expr::Bool(b) => {
                self.code.push(Instruction::I64Const(i64::from(*b)));
                Ok(Bool)
            }
            Expr::Nil => {
                self.code.push(Instruction::I64Const(0));
                Ok(Nil)
            }
            Expr::Str(s) => {
                let offset = self.builder.data.len();
                self.builder.data.extend_from_slice(s.as_bytes());
                ensure!(
                    self.builder.data.len() <= 8 * 1024 * 1024,
                    "literal memory exceeds limit"
                );
                self.code.extend([
                    Instruction::I64Const(offset as i64),
                    Instruction::I64Const(s.len() as i64),
                ]);
                self.call_builtin("literal")
            }
            Expr::Var(n) => {
                let (id, ty) = self
                    .vars
                    .get(n)
                    .with_context(|| format!("unknown variable {n}"))?;
                self.code.push(Instruction::LocalGet(*id));
                Ok(*ty)
            }
            Expr::Call(name, args) => {
                ensure!(name != "literal", "literal is reserved");
                let types = args
                    .iter()
                    .map(|e| self.expr(e))
                    .collect::<Result<Vec<_>>>()?;
                if let Some((id, b)) = BUILTINS.iter().enumerate().find(|(_, b)| b.name == name) {
                    ensure!(
                        b.args.len() == types.len()
                            && b.args.iter().zip(&types).all(|(a, b)| compatible(*a, *b)),
                        "wrong arguments for {name}"
                    );
                    self.code.push(Instruction::Call(id as u32));
                    Ok(b.result)
                } else {
                    let (id, ty) = self.builder.function(name, &types)?;
                    self.code.push(Instruction::Call(id));
                    Ok(ty)
                }
            }
            Expr::Unary(op, e) => {
                if op == "-" {
                    self.code.push(Instruction::I64Const(0));
                    ensure!(self.expr(e)? == Int, "negation requires integer");
                    self.code.push(Instruction::I64Sub);
                    Ok(Int)
                } else {
                    ensure!(self.expr(e)? == Bool, "not requires boolean");
                    self.code
                        .extend([Instruction::I64Eqz, Instruction::I64ExtendI32U]);
                    Ok(Bool)
                }
            }
            Expr::Binary(op, a, b) => {
                let left = self.expr(a)?;
                if op == "and" || op == "or" {
                    ensure!(left == Bool, "logical operands must be boolean");
                    self.code.extend([
                        Instruction::I64Eqz,
                        Instruction::I32Eqz,
                        Instruction::If(BlockType::Result(ValType::I64)),
                    ]);
                    if op == "and" {
                        ensure!(self.expr(b)? == Bool, "logical operands must be boolean");
                    } else {
                        self.code.push(Instruction::I64Const(1));
                    }
                    self.code.push(Instruction::Else);
                    if op == "or" {
                        ensure!(self.expr(b)? == Bool, "logical operands must be boolean");
                    } else {
                        self.code.push(Instruction::I64Const(0));
                    }
                    self.code.push(Instruction::End);
                    return Ok(Bool);
                }
                let right = self.expr(b)?;
                if op == ".." {
                    ensure!(
                        left == String && right == String,
                        "concatenation requires strings"
                    );
                    return self.call_builtin("str.concat");
                }
                if ["==", "~="].contains(&op.as_str()) {
                    ensure!(
                        compatible(left, right) || compatible(right, left),
                        "cannot compare different types"
                    );
                    if matches!(left, String | Nil) && matches!(right, String | Nil) {
                        self.call_builtin("str.eq")?;
                        if op == "~=" {
                            self.code
                                .extend([Instruction::I64Eqz, Instruction::I64ExtendI32U]);
                        }
                    } else {
                        self.code.push(if op == "==" {
                            Instruction::I64Eq
                        } else {
                            Instruction::I64Ne
                        });
                        self.code.push(Instruction::I64ExtendI32U);
                    }
                    return Ok(Bool);
                }
                ensure!(
                    left == Int && right == Int,
                    "operator {op} requires integers"
                );
                let (instruction, comparison) = match op.as_str() {
                    "+" => (Instruction::I64Add, false),
                    "-" => (Instruction::I64Sub, false),
                    "*" => (Instruction::I64Mul, false),
                    "/" => (Instruction::I64DivS, false),
                    "%" => (Instruction::I64RemS, false),
                    "<" => (Instruction::I64LtS, true),
                    ">" => (Instruction::I64GtS, true),
                    "<=" => (Instruction::I64LeS, true),
                    ">=" => (Instruction::I64GeS, true),
                    _ => bail!("unsupported operator {op}"),
                };
                self.code.push(instruction);
                if comparison {
                    self.code.push(Instruction::I64ExtendI32U);
                }
                Ok(if comparison { Bool } else { Int })
            }
        }
    }
}
