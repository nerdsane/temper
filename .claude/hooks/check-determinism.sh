#!/bin/bash
# Determinism Guard (PostToolUse — Write|Edit)
# BLOCKING: YES (exit 2 on violation)
#
# After editing .rs files in simulation-visible crates, scans for
# non-deterministic patterns that break DST reproducibility.
#
# Based on FoundationDB, TigerBeetle, S2, and Polar Signals DST practices.
# See docs/HARNESS.md for the full rationale.
#
# Suppress false positives with: // determinism-ok
set -euo pipefail

PAYLOAD="$(cat)"

# Extract file path from tool input
if command -v jq >/dev/null 2>&1; then
    FILE_PATH="$(echo "$PAYLOAD" | jq -r '.tool_input.file_path // .tool_input.path // empty')"
else
    FILE_PATH="$(echo "$PAYLOAD" | grep -o '"file_path"[[:space:]]*:[[:space:]]*"[^"]*"' | head -1 | sed 's/.*"\([^"]*\)"$/\1/' || true)"
fi

# Only check .rs files
case "${FILE_PATH:-}" in
    *.rs) ;;
    *) exit 0 ;;
esac

# Only check simulation-visible crates
case "${FILE_PATH:-}" in
    */temper-runtime/*|*/temper-jit/*|*/temper-server/*) ;;
    *) exit 0 ;;
esac

# Skip test files
case "${FILE_PATH:-}" in
    */tests/*|*_test.rs|*test_*.rs) exit 0 ;;
esac

# Helper: check for a pattern, excluding determinism-ok, comments, and trailing comments
check_pattern() {
    local pattern="$1"
    local message="$2"

    local matches
    matches="$(grep -n "$pattern" "$FILE_PATH" 2>/dev/null || true)"
    [ -z "$matches" ] && return

    # Exclude determinism-ok annotations
    matches="$(echo "$matches" | grep -v '// determinism-ok' || true)"
    [ -z "$matches" ] && return

    # Exclude pure comment lines (grep -n prefixes "NNN:", so anchor after it)
    matches="$(echo "$matches" | grep -v '^[0-9]*:[[:space:]]*//' || true)"
    [ -z "$matches" ] && return

    # Only flag if pattern appears in code, not just in a trailing comment
    while IFS= read -r line; do
        local content="${line#*:}"
        # Strip trailing line comment (space + //) to isolate code portion
        local code_part="${content%% //*}"
        if echo "$code_part" | grep -q "$pattern" 2>/dev/null; then
            VIOLATIONS="${VIOLATIONS}\n  BLOCKED: $message"
            HAS_VIOLATION=true
            return
        fi
    done <<< "$matches"
}

VIOLATIONS=""
HAS_VIOLATION=false

# ═══════════════════════════════════════════════════════════════
# CATEGORY 1: Collections — Non-deterministic iteration order
# FoundationDB: "BTreeMap, not HashMap" (explicit rule)
# TigerBeetle: Custom deterministic collections throughout
# ═══════════════════════════════════════════════════════════════

check_pattern 'HashMap' \
    "HashMap found — use BTreeMap for deterministic iteration order"

check_pattern 'HashSet' \
    "HashSet found — use BTreeSet for deterministic iteration order"

check_pattern 'DashMap' \
    "DashMap found — concurrent hash map with non-deterministic ordering"

check_pattern 'FuturesUnordered' \
    "FuturesUnordered found — completion order is non-deterministic"

check_pattern 'IndexMap\|IndexSet' \
    "IndexMap/IndexSet found — uses random SipHash unless configured with fixed hasher"

# ═══════════════════════════════════════════════════════════════
# CATEGORY 2: Time and Clocks
# FoundationDB: All code must use g_network->now()
# TigerBeetle: TimeSim provides virtual time
# ═══════════════════════════════════════════════════════════════

check_pattern 'SystemTime::now\|Instant::now' \
    "SystemTime::now()/Instant::now() found — use sim_now() for simulation-safe time"

check_pattern 'chrono::Utc::now\|chrono::Local::now\|OffsetDateTime::now' \
    "chrono/time real clock access found — use sim_now()"

check_pattern 'std::thread::sleep\b' \
    "std::thread::sleep() found — blocks thread, breaks simulation timing"

check_pattern 'tokio::time::sleep\b' \
    "tokio::time::sleep() found — use simulated time / sim_now()"

# ═══════════════════════════════════════════════════════════════
# CATEGORY 3: Randomness and Entropy
# FoundationDB: Mandatory deterministicRandom() seeded PRNG
# TigerBeetle: All std.Random replaced with stdx.PRNG
# ═══════════════════════════════════════════════════════════════

check_pattern 'Uuid::new_v4' \
    "Uuid::new_v4() found — use sim_uuid() for deterministic UUIDs"

check_pattern 'thread_rng\|rand::random\b\|OsRng' \
    "Unseeded RNG found — use seeded PRNG for reproducibility"

check_pattern 'getrandom' \
    "getrandom found — system entropy source breaks determinism"

# ═══════════════════════════════════════════════════════════════
# CATEGORY 4: Threading and Concurrency
# FoundationDB: Single-threaded, cooperative multitasking only
# TigerBeetle: Single-threaded event loop, deterministic ticking
# Polar Signals: async keyword banned from state machine traits
# ═══════════════════════════════════════════════════════════════

check_pattern 'std::thread::spawn' \
    "std::thread::spawn() found — use actor model / message passing"

check_pattern 'rayon::' \
    "rayon parallel iteration found — use sequential iteration in simulation"

check_pattern 'tokio::spawn\b' \
    "tokio::spawn() found — task scheduling is non-deterministic on multi-threaded runtime"

# ═══════════════════════════════════════════════════════════════
# CATEGORY 5: I/O and System Calls
# FoundationDB: Interface swapping via INetwork (Net2/Sim2)
# TigerBeetle: All I/O mocked — network + storage simulators
# ═══════════════════════════════════════════════════════════════

check_pattern 'std::fs::' \
    "std::fs found — real filesystem I/O breaks simulation determinism"

check_pattern 'std::net::' \
    "std::net found — real network I/O breaks simulation determinism"

check_pattern 'std::env::var\b' \
    "std::env::var() found — environment varies per machine/run"

check_pattern 'std::process::id\b' \
    "std::process::id() found — PID differs every run"

# ═══════════════════════════════════════════════════════════════
# CATEGORY 6: Global Mutable State
# TigerBeetle: Static allocation only, no runtime allocation
# FoundationDB: Controlled globals only (g_network, g_simulator)
# ═══════════════════════════════════════════════════════════════

check_pattern 'static mut ' \
    "static mut found — unsound, initialization order undefined"

check_pattern 'lazy_static!' \
    "lazy_static! found — first-access initialization is order-dependent"

check_pattern 'thread_local!' \
    "thread_local! found — state isolated per thread, breaks simulation"

# ═══════════════════════════════════════════════════════════════
# CATEGORY 7: Serialization Hazards
# TigerBeetle: Custom copy_disjoint, zero-padded buffers
# ═══════════════════════════════════════════════════════════════

check_pattern 'sort_unstable\b' \
    "sort_unstable() found — relative order of equal elements is arbitrary, use sort()"

# ═══════════════════════════════════════════════════════════════
# Report results
# ═══════════════════════════════════════════════════════════════

if [ "$HAS_VIOLATION" = true ]; then
    echo "" >&2
    echo "══════════════════════════════════════════════════════════════" >&2
    echo "  DETERMINISM VIOLATION in $(basename "$FILE_PATH")" >&2
    echo "══════════════════════════════════════════════════════════════" >&2
    echo -e "$VIOLATIONS" >&2
    echo "" >&2
    echo "  These patterns break deterministic simulation reproducibility." >&2
    echo "  Ref: FoundationDB, TigerBeetle, S2 DST best practices." >&2
    echo "  Add '// determinism-ok' on the line to suppress false positives." >&2
    echo "══════════════════════════════════════════════════════════════" >&2
    exit 2
fi

exit 0
