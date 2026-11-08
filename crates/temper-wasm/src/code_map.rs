//! Where compiled guest code lives in memory, so a profiler can name a
//! JIT-compiled wasm frame.
//!
//! A native profiler resolves an instruction address through the loaded
//! objects' symbol tables. Wasmtime's compiled code sits in anonymous memory
//! that no symbol table covers, so those addresses resolve to nothing and the
//! frame is lost. Every compiled module adds its code range here for as long
//! as it is alive ([`CodeRange`] removes it on drop), and [`symbol_at`] turns
//! an address inside one into the module and function it belongs to.
//!
//! Lookups take a read lock and two binary searches; writes happen only when a
//! module is compiled or dropped.

use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

use wasmtime::Module;

/// A guest function an address resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmSymbol {
    /// The module's own name, or `wasm:<first 12 of its hash>` when it has none.
    pub module: String,
    /// The function's name from the module's name section (still mangled, as
    /// a native symbol table holds it), or `function[<index>]` without one.
    pub function: String,
}

/// One module's code: where it ends and its functions, sorted by start offset.
struct ModuleCode {
    end: usize,
    module: String,
    /// `(offset from code start, length, name)`, sorted by offset.
    functions: Vec<(usize, usize, String)>,
}

/// Live code ranges by start address. Only observability reads it.
fn ranges() -> &'static RwLock<BTreeMap<usize, ModuleCode>> {
    static RANGES: OnceLock<RwLock<BTreeMap<usize, ModuleCode>>> = OnceLock::new(); // determinism-ok: profiler symbol table, never read by simulated code
    RANGES.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Keeps a module's code range in the map; removes it when dropped.
pub(crate) struct CodeRange {
    start: usize,
}

impl CodeRange {
    /// Record `module`'s compiled code, labelled by its name or `hash`.
    pub(crate) fn register(module: &Module, hash: &str) -> Self {
        let text = module.text();
        let start = text.as_ptr() as usize;
        let label = match module.name() {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => format!("wasm:{}", &hash[..hash.len().min(12)]),
        };
        let mut functions: Vec<(usize, usize, String)> = module
            .functions()
            .map(|f| {
                let name = f
                    .name
                    .unwrap_or_else(|| format!("function[{}]", f.index.as_u32()));
                (f.offset, f.len, name)
            })
            .collect();
        functions.sort_by_key(|(offset, _, _)| *offset);
        debug_assert!(
            functions.iter().all(|(o, l, _)| o + l <= text.len()),
            "every function lies inside the module's text"
        );
        let code = ModuleCode {
            end: start + text.len(),
            module: label,
            functions,
        };
        ranges()
            .write()
            .expect("wasm code map poisoned")
            .insert(start, code);
        Self { start }
    }
}

impl Drop for CodeRange {
    fn drop(&mut self) {
        if let Ok(mut map) = ranges().write() {
            map.remove(&self.start);
        }
    }
}

/// The guest function holding instruction address `ip`, if any live module's
/// compiled code contains it.
pub fn symbol_at(ip: usize) -> Option<WasmSymbol> {
    let map = ranges().read().ok()?;
    let (start, code) = map.range(..=ip).next_back()?;
    if ip >= code.end {
        return None;
    }
    let offset = ip - start;
    let at = code.functions.partition_point(|(o, _, _)| *o <= offset);
    let function = at
        .checked_sub(1)
        .map(|i| &code.functions[i])
        .filter(|(o, len, _)| offset < o + len)
        .map(|(_, _, name)| name.clone())
        .unwrap_or_else(|| "[trampoline]".to_string());
    Some(WasmSymbol {
        module: code.module.clone(),
        function,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAT: &str = r#"(module $sample
        (func $first (result i32) i32.const 1)
        (func $second (export "second") (result i32) call $first i32.const 2 i32.add))"#;

    #[test]
    fn addresses_inside_a_live_module_name_its_functions() {
        let engine = wasmtime::Engine::default();
        let module = Module::new(&engine, WAT).expect("module compiles");
        let start = module.text().as_ptr() as usize;
        let mut named: Vec<(usize, String)> = module
            .functions()
            .map(|f| (f.offset, f.name.unwrap_or_default()))
            .collect();
        named.sort();

        let range = CodeRange::register(&module, "abcdef0123456789");
        for (offset, name) in &named {
            let symbol = symbol_at(start + offset).expect("inside the module");
            assert_eq!(symbol.module, "sample");
            assert_eq!(&symbol.function, name);
        }
        assert!(named.iter().any(|(_, n)| n == "first"));
        assert!(named.iter().any(|(_, n)| n == "second"));
        assert_eq!(symbol_at(start + module.text().len()), None);

        drop(range);
        assert_eq!(symbol_at(start), None, "a dropped module leaves the map");
    }
}
