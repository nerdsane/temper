//! Generic Monty sandbox runner.
//!
//! Provides [`run_sandbox`] which eliminates the duplicated Monty `RunProgress`
//! loop from `temper-mcp`.

use std::collections::BTreeMap;
use std::future::Future;

use anyhow::{Context, Result, anyhow, bail};
use monty::{
    DictPairs, ExcType, ExtFunctionResult, LimitedTracker, MontyException, MontyObject, MontyRepl,
    MontyRun, NameLookupResult, PrintWriter, ReplProgress, ReplStartError, RunProgress,
};
use serde_json::Value;

use crate::convert::{json_to_monty_object, monty_object_to_json};
use crate::helpers::{default_limits, format_monty_exception, wrap_user_code};

/// Run Python code in the Monty sandbox with the given dataclass objects and dispatch closure.
///
/// - `code`: User Python code to execute
/// - `filename`: Filename for error messages (e.g. `"execute.py"`, `"agent.py"`)
/// - `dataclasses`: Named dataclass objects to inject as positional args.
///   Each tuple is `(param_name, dataclass_name, type_id)`.
/// - `dispatch`: Async closure called for each external method call.
///   Receives `(function_name, args, kwargs)` and returns `Result<Value, String>`.
pub async fn run_sandbox<F, Fut>(
    code: &str,
    filename: &str,
    dataclasses: &[(&str, &str, u64)],
    dispatch: F,
) -> Result<String>
where
    F: Fn(String, Vec<MontyObject>, Vec<(MontyObject, MontyObject)>) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    let program = wrap_user_code(code);
    let param_names: Vec<String> = dataclasses
        .iter()
        .map(|(name, _, _)| name.to_string())
        .collect();
    let runner = MontyRun::new(program, filename, param_names)
        .map_err(|e| anyhow!(format_monty_exception(&e)))?;

    let objects: Vec<MontyObject> = dataclasses
        .iter()
        .map(|(_, class_name, type_id)| MontyObject::Dataclass {
            name: class_name.to_string(),
            type_id: *type_id,
            field_names: vec![],
            attrs: DictPairs::from(Vec::<(MontyObject, MontyObject)>::new()),
            frozen: true,
        })
        .collect();

    let tracker = LimitedTracker::new(default_limits());

    // Start the Monty program. PrintWriter is created in its own scope
    // so it's dropped before any async work, keeping the future Send.
    let mut progress = {
        let mut print = PrintWriter::Disabled;
        runner
            .start(objects, tracker, &mut print)
            .map_err(|e| anyhow!(format_monty_exception(&e)))?
    };

    let mut pending_results: BTreeMap<u32, ExtFunctionResult> = BTreeMap::new();

    loop {
        match progress {
            RunProgress::Complete(result) => {
                let value = monty_object_to_json(&result);
                return serde_json::to_string(&value)
                    .context("failed to serialize sandbox output as JSON string");
            }
            RunProgress::FunctionCall(call) => {
                if !call.method_call {
                    bail!(
                        "sandbox denied external function call '{}'. \
                         Only dataclass.<method> calls are allowed",
                        call.function_name
                    );
                }

                let call_id = call.call_id;
                let fn_name = call.function_name.clone();
                let args = call.args.clone();
                let kwargs = call.kwargs.clone();
                let result = dispatch(fn_name, args, kwargs).await;
                let ext_result = match result {
                    Ok(value) => ExtFunctionResult::Return(json_to_monty_object(&value)),
                    Err(message) => ExtFunctionResult::Error(MontyException::new(
                        ExcType::RuntimeError,
                        Some(message),
                    )),
                };

                pending_results.insert(call_id, ext_result);
                let mut print = PrintWriter::Disabled;
                progress = call
                    .resume(ExtFunctionResult::Future(call_id), &mut print)
                    .map_err(|e| anyhow!(format_monty_exception(&e)))?;
            }
            RunProgress::ResolveFutures(state) => {
                let mut ready: Vec<(u32, ExtFunctionResult)> = Vec::new();
                for call_id in state.pending_call_ids() {
                    if let Some(result) = pending_results.remove(call_id) {
                        ready.push((*call_id, result));
                    }
                }

                if ready.is_empty() {
                    bail!(
                        "sandbox is waiting on unresolved external calls: {:?}",
                        state.pending_call_ids()
                    );
                }

                let mut print = PrintWriter::Disabled;
                progress = state
                    .resume(ready, &mut print)
                    .map_err(|e| anyhow!(format_monty_exception(&e)))?;
            }
            RunProgress::NameLookup(lookup) => {
                let mut print = PrintWriter::Disabled;
                progress = lookup
                    .resume(NameLookupResult::Undefined, &mut print)
                    .map_err(|e| anyhow!(format_monty_exception(&e)))?;
            }
            RunProgress::OsCall(os_call) => {
                bail!(
                    "sandbox blocked OS access ({:?}). \
                     Filesystem/network/env access is disabled in the sandbox.",
                    os_call.function
                );
            }
        }
    }
}

/// Persistent sandbox that preserves heap and global variables between executions.
///
/// Uses [`MontyRepl`] to maintain state across snippet executions. Each call to
/// [`execute`](Self::execute) runs only the new code against accumulated state —
/// variables, functions, and heap mutations all survive between calls.
pub struct PersistentSandbox {
    repl: Option<MontyRepl<LimitedTracker>>,
    dataclasses: Vec<(&'static str, &'static str, u64)>,
}

impl PersistentSandbox {
    /// Create a new persistent sandbox with the given dataclass objects.
    pub fn new(dataclasses: &[(&'static str, &'static str, u64)]) -> Self {
        Self {
            repl: None,
            dataclasses: dataclasses.to_vec(),
        }
    }

    /// Execute code against persistent state. Variables survive across calls.
    pub async fn execute<F, Fut>(&mut self, code: &str, dispatch: F) -> Result<String>
    where
        F: Fn(String, Vec<MontyObject>, Vec<(MontyObject, MontyObject)>) -> Fut,
        Fut: Future<Output = Result<Value, String>>,
    {
        let repl = match self.repl.take() {
            Some(repl) => repl,
            None => self.init_repl()?,
        };

        let program = wrap_user_code(code);
        let mut print = PrintWriter::Disabled;
        let progress = match repl.start(&program, &mut print) {
            Ok(progress) => progress,
            Err(err) => {
                let ReplStartError { repl, error } = *err;
                self.repl = Some(repl);
                bail!(format_monty_exception(&error));
            }
        };

        self.drive_repl_progress(progress, dispatch).await
    }

    /// Initialize the REPL with dataclass inputs and empty code.
    fn init_repl(&self) -> Result<MontyRepl<LimitedTracker>> {
        let input_names: Vec<String> = self
            .dataclasses
            .iter()
            .map(|(name, _, _)| name.to_string())
            .collect();
        let inputs: Vec<MontyObject> = self
            .dataclasses
            .iter()
            .map(|(_, class_name, type_id)| MontyObject::Dataclass {
                name: class_name.to_string(),
                type_id: *type_id,
                field_names: vec![],
                attrs: DictPairs::from(Vec::<(MontyObject, MontyObject)>::new()),
                frozen: true,
            })
            .collect();

        let tracker = LimitedTracker::new(default_limits());
        let mut print = PrintWriter::Disabled;
        let (repl, _) = MontyRepl::new(
            String::new(),
            "execute.py",
            input_names,
            inputs,
            tracker,
            &mut print,
        )
        .map_err(|e| anyhow!(format_monty_exception(&e)))?;

        Ok(repl)
    }

    /// Drive the [`ReplProgress`] state machine to completion, handling external calls.
    async fn drive_repl_progress<F, Fut>(
        &mut self,
        mut progress: ReplProgress<LimitedTracker>,
        dispatch: F,
    ) -> Result<String>
    where
        F: Fn(String, Vec<MontyObject>, Vec<(MontyObject, MontyObject)>) -> Fut,
        Fut: Future<Output = Result<Value, String>>,
    {
        let mut pending_results: BTreeMap<u32, ExtFunctionResult> = BTreeMap::new();

        loop {
            match progress {
                ReplProgress::Complete { repl, value } => {
                    self.repl = Some(repl);
                    let json_value = monty_object_to_json(&value);
                    return serde_json::to_string(&json_value)
                        .context("failed to serialize sandbox output as JSON string");
                }
                ReplProgress::FunctionCall(call) => {
                    if !call.method_call {
                        // Deny non-method external calls. Resume with an error so the
                        // REPL can recover (rather than bailing immediately).
                        let err_exc = MontyException::new(
                            ExcType::RuntimeError,
                            Some(format!(
                                "sandbox denied external function call '{}'. \
                                 Only dataclass.<method> calls are allowed",
                                call.function_name
                            )),
                        );
                        let mut print = PrintWriter::Disabled;
                        match call
                            .resume(ExtFunctionResult::Error(err_exc), &mut print)
                        {
                            Ok(p) => {
                                progress = p;
                                continue;
                            }
                            Err(err) => {
                                let ReplStartError { repl, error } = *err;
                                self.repl = Some(repl);
                                bail!(format_monty_exception(&error));
                            }
                        }
                    }

                    let call_id = call.call_id;
                    let fn_name = call.function_name.clone();
                    let args = call.args.clone();
                    let kwargs = call.kwargs.clone();
                    let result = dispatch(fn_name, args, kwargs).await;
                    let ext_result = match result {
                        Ok(value) => ExtFunctionResult::Return(json_to_monty_object(&value)),
                        Err(message) => ExtFunctionResult::Error(MontyException::new(
                            ExcType::RuntimeError,
                            Some(message),
                        )),
                    };

                    pending_results.insert(call_id, ext_result);
                    let mut print = PrintWriter::Disabled;
                    match call.resume(ExtFunctionResult::Future(call_id), &mut print) {
                        Ok(p) => progress = p,
                        Err(err) => {
                            let ReplStartError { repl, error } = *err;
                            self.repl = Some(repl);
                            bail!(format_monty_exception(&error));
                        }
                    }
                }
                ReplProgress::ResolveFutures(state) => {
                    let mut ready: Vec<(u32, ExtFunctionResult)> = Vec::new();
                    for call_id in state.pending_call_ids() {
                        if let Some(result) = pending_results.remove(call_id) {
                            ready.push((*call_id, result));
                        }
                    }

                    if ready.is_empty() {
                        bail!(
                            "sandbox is waiting on unresolved external calls: {:?}",
                            state.pending_call_ids()
                        );
                    }

                    let mut print = PrintWriter::Disabled;
                    match state.resume(ready, &mut print) {
                        Ok(p) => progress = p,
                        Err(err) => {
                            let ReplStartError { repl, error } = *err;
                            self.repl = Some(repl);
                            bail!(format_monty_exception(&error));
                        }
                    }
                }
                ReplProgress::NameLookup(lookup) => {
                    let mut print = PrintWriter::Disabled;
                    match lookup.resume(NameLookupResult::Undefined, &mut print) {
                        Ok(p) => progress = p,
                        Err(err) => {
                            let ReplStartError { repl, error } = *err;
                            self.repl = Some(repl);
                            bail!(format_monty_exception(&error));
                        }
                    }
                }
                ReplProgress::OsCall(os_call) => {
                    let err_exc = MontyException::new(
                        ExcType::RuntimeError,
                        Some(format!(
                            "sandbox blocked OS access ({:?}). \
                             Filesystem/network/env access is disabled in the sandbox.",
                            os_call.function
                        )),
                    );
                    let mut print = PrintWriter::Disabled;
                    match os_call.resume(ExtFunctionResult::Error(err_exc), &mut print) {
                        Ok(p) => {
                            progress = p;
                            continue;
                        }
                        Err(err) => {
                            let ReplStartError { repl, error } = *err;
                            self.repl = Some(repl);
                            bail!(format_monty_exception(&error));
                        }
                    }
                }
            }
        }
    }
}
