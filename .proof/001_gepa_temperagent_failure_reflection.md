# 001_gepa_temperagent_failure_reflection

Date: 2026-03-23
Branch: docs/positioning-rewrite
Scope: TemperAgent sandbox provisioning failure reflection + root-cause hardening

## Problem
`sandbox_provisioner` previously swallowed TemperFS bootstrap errors and still emitted `SandboxReady` with empty IDs. This made failures look like partial success.

## Code Changes
- Hard fail on TemperFS bootstrap failure with explicit error context (`temper_api_url`, tenant, agent id).
- Added `temper_api_url` to TemperAgent state + `Configure` action.
- Updated CSDL to expose `TemperApiUrl` and `Configure.temper_api_url`.
- All TemperAgent WASM modules now resolve Temper API URL from entity `fields.temper_api_url` first, then integration config.
- Installing `temper-agent` now auto-installs `temper-fs` dependency.
- Added regression test for dependency install behavior.

## Verification
### Build/Test
- `cargo fmt --all`
- `cargo check -p temper-platform`
- `cargo test -p temper-platform os_apps::tests::test_install_temper_agent_auto_installs_temper_fs -- --nocapture` (passed)
- Built all TemperAgent WASM modules for `wasm32-unknown-unknown`.

### Live Proof (port 3015)
Tenant: `proof-fix-20260323`

Prereq: Uploaded tenant-scoped WASM modules:
- `sandbox_provisioner`
- `llm_caller`
- `tool_runner`
- `workspace_restorer`

Case A (bad API URL):
- Agent: `019d1c5a-4a43-71b0-adc9-199cb90eefab`
- Configure with `temper_api_url=http://127.0.0.1:39999`
- Provision result:
  - `status=Failed`
  - `error_message="TemperFS bootstrap failed at http://127.0.0.1:39999/tdata ... Ensure os-app 'temper-fs' is installed ... temper_api_url is correct."`
  - `workspace_id`, `conversation_file_id`, `file_manifest_id` all empty

Case B (correct API URL):
- Agent: `019d1c5a-4f84-7d23-8c18-dfc23885e3b9`
- Configure with `temper_api_url=http://127.0.0.1:3015`
- Provision + callback result:
  - `status=Failed` (intentional due `max_turns=0`)
  - `error_message="turn budget exhausted (0/0)"`
  - `workspace_id`, `conversation_file_id`, `file_manifest_id` all populated

Interpretation:
- Failure reflection is now explicit and truthful for TemperFS bootstrap issues.
- With correct URL, TemperFS bootstrap succeeds and IDs are present.

## Operational Caveat
If tenant-scoped WASM modules are not uploaded, integration dispatch fails with `WASM module '<name>' not found`. This is separate from TemperFS bootstrap handling.
