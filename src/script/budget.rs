use anyhow::{Result, ensure};
use wasmparser::{Operator, Parser, Payload};

pub(super) fn validate(bytes: &[u8]) -> Result<()> {
    let mut instructions = 0usize;
    let mut total_locals = 0u64;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload? {
            Payload::TypeSection(types) => {
                ensure!(types.count() <= 1024, "Wasm exceeds 1024 types");
                for function in types.into_iter_err_on_gc_types() {
                    let function = function?;
                    ensure!(
                        function.params().len() <= 64 && function.results().len() <= 1,
                        "Wasm function signature exceeds compilation budget"
                    );
                }
            }
            Payload::ImportSection(imports) => {
                ensure!(imports.count() <= 64, "Wasm exceeds 64 imports")
            }
            Payload::FunctionSection(functions) => {
                ensure!(functions.count() <= 1024, "Wasm exceeds 1024 functions")
            }
            Payload::GlobalSection(globals) => {
                ensure!(globals.count() <= 1024, "Wasm exceeds 1024 globals")
            }
            Payload::TableSection(tables) => {
                ensure!(tables.count() == 0, "Wasm tables are unsupported")
            }
            Payload::MemorySection(memories) => {
                ensure!(memories.count() <= 1, "Wasm exceeds one memory");
                for memory in memories {
                    let memory = memory?;
                    ensure!(
                        !memory.memory64 && !memory.shared && memory.initial <= 128,
                        "Wasm requires unshared 32-bit memory at most 8 MiB"
                    );
                }
            }
            Payload::CodeSectionEntry(body) => {
                ensure!(
                    body.range().len() <= 64 * 1024,
                    "Wasm function body exceeds 64 KiB"
                );
                let mut locals = 0u64;
                for local in body.get_locals_reader()? {
                    locals += u64::from(local?.0);
                    ensure!(locals <= 4096, "Wasm function exceeds 4096 locals");
                }
                total_locals += locals;
                ensure!(total_locals <= 16384, "Wasm module exceeds 16384 locals");
                let mut depth = 0usize;
                let mut reader = body.get_operators_reader()?;
                while !reader.eof() {
                    instructions += 1;
                    ensure!(
                        instructions <= 50_000,
                        "Wasm exceeds compilation instruction budget"
                    );
                    match reader.read()? {
                        Operator::Block { .. } | Operator::Loop { .. } | Operator::If { .. } => {
                            depth += 1;
                            ensure!(depth <= 128, "Wasm control nesting exceeds 128");
                        }
                        Operator::End => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}
