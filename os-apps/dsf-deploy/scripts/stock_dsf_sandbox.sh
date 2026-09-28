#!/usr/bin/env bash
# Stock TensorLake sandbox `dsf` (also called dd comp) for Deep Sci-Fi.
#
# Installs the official Datadog `pup` CLI when missing and checks that
# Datadog / Temper env *names* are present. Does not write secrets, print
# secret values, bounce Railway, or publish Galley.
#
# Official pup sources (do not invent asset names):
#   https://github.com/DataDog/pup
#   https://docs.datadoghq.com/cli/
#
# Usage (on the dsf sandbox, or any host that should carry the same tools):
#   ./os-apps/dsf-deploy/scripts/stock_dsf_sandbox.sh
# Optional:
#   STOCK_DSF_BUILD_PUP=1  build pup from source when brew is unavailable

set -euo pipefail

ok() { printf 'OK   %s\n' "$1"; }
miss() { printf 'MISS %s\n' "$1"; }
info() { printf 'INFO %s\n' "$1"; }

present() {
    local name="$1"
    if [ -n "${!name:-}" ]; then
        ok "$name is set (value not printed)"
        return 0
    fi
    miss "$name"
    return 1
}

install_pup() {
    if command -v pup >/dev/null 2>&1; then
        ok "pup is on PATH: $(command -v pup)"
        return 0
    fi

    info "pup is not on PATH"

    if command -v brew >/dev/null 2>&1; then
        info "installing pup via Homebrew (datadog-labs/pack/pup)"
        brew tap datadog-labs/pack
        brew install datadog-labs/pack/pup
        if command -v pup >/dev/null 2>&1; then
            ok "pup installed via Homebrew"
            return 0
        fi
        miss "brew install finished but pup is still not on PATH"
        return 1
    fi

    if [ "${STOCK_DSF_BUILD_PUP:-}" = "1" ] && command -v cargo >/dev/null 2>&1; then
        local workdir
        workdir="$(mktemp -d)"
        info "building pup from source into $workdir (STOCK_DSF_BUILD_PUP=1)"
        git clone --depth 1 https://github.com/DataDog/pup.git "$workdir/pup"
        (cd "$workdir/pup" && cargo build --release)
        local dest="${STOCK_DSF_PUP_BIN:-/usr/local/bin/pup}"
        if [ -w "$(dirname "$dest")" ]; then
            cp "$workdir/pup/target/release/pup" "$dest"
            ok "pup built and copied to $dest"
            return 0
        fi
        info "built $workdir/pup/target/release/pup — copy it onto PATH yourself"
        return 0
    fi

    miss "pup is not installed"
    info "Install with one of:"
    info "  brew tap datadog-labs/pack && brew install datadog-labs/pack/pup"
    info "  git clone https://github.com/DataDog/pup.git && cd pup && cargo build --release"
    info "  download a binary from https://github.com/DataDog/pup/releases/latest"
    info "Or re-run with STOCK_DSF_BUILD_PUP=1 when cargo is available."
    return 1
}

check_datadog_env() {
    local auth_ok=0
    local site_ok=0

    if present DD_SITE; then
        site_ok=1
    else
        info "DD_SITE unset; pup defaults to datadoghq.com"
    fi

    if present DD_ACCESS_TOKEN; then
        auth_ok=1
    elif present DD_API_KEY && present DD_APP_KEY; then
        auth_ok=1
    else
        miss "Datadog auth: set DD_ACCESS_TOKEN, or both DD_API_KEY and DD_APP_KEY"
    fi

    if [ -n "${PUP_TRUST_SITE:-}" ]; then
        ok "PUP_TRUST_SITE is set (value not printed)"
    else
        info "PUP_TRUST_SITE unset (optional)"
    fi

    if [ "$auth_ok" -eq 1 ]; then
        ok "Datadog auth env is present"
    fi

    # site is recommended, not required (pup has a default)
    if [ "$auth_ok" -eq 1 ] && [ "$site_ok" -eq 1 ]; then
        return 0
    fi
    if [ "$auth_ok" -eq 1 ]; then
        return 0
    fi
    return 1
}

check_temper_connect() {
    if [ -n "${TEMPER_SANDBOX_NAME:-}" ]; then
        ok "TEMPER_SANDBOX_NAME is set (value not printed)"
        if [ "${TEMPER_SANDBOX_NAME}" = "dsf" ]; then
            ok "TEMPER_SANDBOX_NAME equals dsf"
        else
            info "TEMPER_SANDBOX_NAME is not dsf; Deep Sci-Fi expects dsf"
        fi
    else
        miss "TEMPER_SANDBOX_NAME (expected dsf)"
    fi

    local url_ok=0
    local key_ok=0

    if present TEMPER_SANDBOX_URL; then
        url_ok=1
    else
        info "TEMPER_SANDBOX_URL is required on the Temper host to connect dsf"
    fi

    if present TENSORLAKE_API_KEY; then
        key_ok=1
    else
        info "TENSORLAKE_API_KEY is required on the Temper host for the dsf proxy"
    fi

    if [ "$url_ok" -eq 1 ] && [ "$key_ok" -eq 1 ]; then
        return 0
    fi
    return 1
}

main() {
    info "Stocking Deep Sci-Fi sandbox dsf (dd comp)"
    info "No secret values will be printed or written"

    local status=0
    install_pup || status=1
    check_datadog_env || status=1
    check_temper_connect || status=1

    if [ "$status" -eq 0 ]; then
        ok "dsf stock check passed"
    else
        miss "dsf stock check failed — fix MISS lines and re-run"
    fi
    return "$status"
}

main "$@"
