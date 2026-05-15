// Cedar policies for the ReadersWriters reference app.
//
// Client principals may request protocol steps. Commit callbacks are reserved
// for the WASM runtime principal so clients cannot forge state updates.

permit(
    principal is Agent,
    action in [
        Action::"read",
        Action::"TryRead",
        Action::"TryWrite",
        Action::"ReadOrWrite",
        Action::"Stop",
        Action::"tryRead",
        Action::"tryWrite",
        Action::"readOrWrite",
        Action::"stop"
    ],
    resource is ReadersWriters
) when {
    principal.role == "lock_client" ||
    principal.role == "operations_agent"
};

permit(
    principal is Agent,
    action in [
        Action::"ValidateProposal",
        Action::"Enqueued",
        Action::"Unchanged",
        Action::"ReaderStartedFromIdle",
        Action::"ReaderStartedFromReading",
        Action::"WriterStarted",
        Action::"ReaderStoppedMoreRemain",
        Action::"ReaderStoppedLast",
        Action::"WriterStopped",
        Action::"Rejected",
        Action::"StepFailed",
        Action::"validateProposal",
        Action::"enqueued",
        Action::"unchanged",
        Action::"readerStartedFromIdle",
        Action::"readerStartedFromReading",
        Action::"writerStarted",
        Action::"readerStoppedMoreRemain",
        Action::"readerStoppedLast",
        Action::"writerStopped",
        Action::"rejected",
        Action::"stepFailed"
    ],
    resource is ReadersWriters
) when {
    principal.role == "wasm_module"
};

permit(
    principal is Admin,
    action,
    resource
);

forbid(
    principal,
    action in [
        Action::"ValidateProposal",
        Action::"Enqueued",
        Action::"ReaderStartedFromIdle",
        Action::"ReaderStartedFromReading",
        Action::"WriterStarted",
        Action::"ReaderStoppedMoreRemain",
        Action::"ReaderStoppedLast",
        Action::"WriterStopped",
        Action::"validateProposal",
        Action::"enqueued",
        Action::"readerStartedFromIdle",
        Action::"readerStartedFromReading",
        Action::"writerStarted",
        Action::"readerStoppedMoreRemain",
        Action::"readerStoppedLast",
        Action::"writerStopped"
    ],
    resource is ReadersWriters
) when {
    !(principal is Agent && principal.role == "wasm_module") &&
    !(principal is Admin)
};
