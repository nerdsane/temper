//! Turn sampled stacks into named frames, wasm guest code included.
//!
//! pprof's own resolver drops every frame no loaded object's symbol table
//! covers, which is all JIT-compiled wasm. This resolves each frame the same
//! way, then asks [`temper_wasm::code_map`] about the frames that stayed
//! unnamed, so a guest function shows under its own name, filed under
//! `wasm/<module>`.

use std::collections::HashMap; // determinism-ok: pprof::Report keys its stacks by HashMap; profiler only
use std::path::PathBuf;

use pprof::{Frames, Report, Symbol, UnresolvedReport};

/// pprof's signal handler: it and the frame under it are the profiler itself.
const SIGNAL_HANDLER: [&str; 2] = ["perf_signal_handler", "_perf_signal_handler"];

/// Resolve every stack of `unresolved` into a report pprof can encode.
pub(super) fn resolve(unresolved: UnresolvedReport) -> Report {
    let mut data: HashMap<Frames, isize> = HashMap::with_capacity(unresolved.data.len()); // determinism-ok: pprof::Report type
    for (stack, count) in unresolved.data {
        let mut frames: Vec<Vec<Symbol>> = Vec::with_capacity(stack.frames.len());
        let mut walk = stack.frames.iter();
        while let Some(frame) = walk.next() {
            let mut symbols: Vec<Symbol> = Vec::new();
            backtrace::resolve_frame(frame, |symbol| {
                symbols.push(Symbol {
                    name: symbol.name().map(|name| name.as_bytes().to_vec()),
                    addr: symbol.addr(),
                    lineno: symbol.lineno(),
                    filename: symbol.filename().map(|path| path.to_owned()),
                });
            });
            if symbols
                .iter()
                .any(|s| SIGNAL_HANDLER.contains(&s.name().as_str()))
            {
                walk.next();
                continue;
            }
            if symbols.is_empty()
                && let Some(guest) = wasm_symbol(frame.ip() as usize)
            {
                symbols.push(guest);
            }
            if !symbols.is_empty() {
                frames.push(symbols);
            }
        }
        let thread_name =
            String::from_utf8_lossy(&stack.thread_name[..stack.thread_name_length]).into_owned();
        let key = Frames {
            frames,
            thread_name,
            thread_id: stack.thread_id,
            sample_timestamp: stack.sample_timestamp,
        };
        // Two raw stacks can resolve alike (e.g. differing only in dropped frames).
        *data.entry(key).or_insert(0) += count;
    }
    Report {
        data,
        timing: unresolved.timing,
    }
}

/// A pprof symbol for a JIT-compiled wasm frame at `ip`, if it is one.
pub(super) fn wasm_symbol(ip: usize) -> Option<Symbol> {
    let guest = temper_wasm::code_map::symbol_at(ip)?;
    Some(Symbol {
        // Name sections hold Rust's mangled names; pprof demangles on encode.
        name: Some(guest.function.into_bytes()),
        addr: Some(ip as *mut std::ffi::c_void),
        lineno: None,
        filename: Some(PathBuf::from(format!("wasm/{}", guest.module))),
    })
}
