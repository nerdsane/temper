"""Crucible Tool Server — unified tool execution for Local and Modal environments.

This FastAPI server handles tool execution for the Crucible agent runtime.
It replaces the previous separate Rust tool server and Python Modal bridge
with a single process:

- **Local** tools execute on the host machine via subprocess / file I/O.
- **Modal** tools execute inside a Modal sandbox via the Modal Python SDK.

The server looks up the environment's `ConfigType` from the Temper OData API
to decide which backend to use. If no `environment_id` is provided, it
defaults to Local execution.

Endpoints:
    GET  /health                       Health check
    POST /tools/{tool_name}            Execute a tool (routes by ConfigType)

Modal sandboxes are provisioned lazily on the first tool call to a Modal
environment — no separate provisioning step is needed.

Startup:
    # Local only (no Modal SDK needed)
    python3 -m uvicorn modal_bridge.server:app --port 3100

    # With Modal support
    MODAL_TOKEN_ID=ak-... MODAL_TOKEN_SECRET=as-... \
    TEMPER_API_URL=http://127.0.0.1:3000 \
    TEMPER_TENANT=crucible \
      python3 -m uvicorn modal_bridge.server:app --port 3100

Requires: fastapi, uvicorn, httpx (see requirements.txt)
Optional: modal (only needed for Modal environments)
"""

from __future__ import annotations

import fnmatch
import logging
import os
import shlex
import subprocess
from pathlib import Path
from typing import Optional

import json
from datetime import datetime, timezone

import httpx
from fastapi import FastAPI, HTTPException
from pydantic import BaseModel

logger = logging.getLogger("crucible_tool_server")
logging.basicConfig(
    level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s"
)

app = FastAPI(title="Crucible Tool Server", version="0.2.0")

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

TEMPER_API_URL = os.environ.get("TEMPER_API_URL", "http://127.0.0.1:3000")
TEMPER_TENANT = os.environ.get("TEMPER_TENANT", "crucible")
MAX_OUTPUT_BYTES = 100 * 1024  # 100KB budget per tool output
MAX_GLOB_RESULTS = 1000

# ---------------------------------------------------------------------------
# Modal sandbox registry (lazy — only imported if Modal envs are used)
# ---------------------------------------------------------------------------

_sandboxes: dict[str, object] = {}  # sandbox_id -> modal.Sandbox
_modal_app: object | None = None
_modal = None  # lazy import


def _ensure_modal():
    """Lazily import modal so the server works without it for Local-only use."""
    global _modal
    if _modal is None:
        try:
            import modal as m

            _modal = m
        except ImportError:
            raise HTTPException(
                status_code=503,
                detail="Modal SDK not installed. Install with: pip install modal",
            )
    return _modal


def _get_modal_app():
    global _modal_app
    modal = _ensure_modal()
    if _modal_app is None:
        _modal_app = modal.App.lookup("crucible-sandboxes", create_if_missing=True)
    return _modal_app


def _get_sandbox(sandbox_id: str):
    modal = _ensure_modal()
    if sandbox_id in _sandboxes:
        return _sandboxes[sandbox_id]
    try:
        sb = modal.Sandbox.from_id(sandbox_id)
        _sandboxes[sandbox_id] = sb
        return sb
    except Exception as e:
        raise HTTPException(
            status_code=404, detail=f"Sandbox {sandbox_id} not found: {e}"
        )


# ---------------------------------------------------------------------------
# Environment lookup (cached)
# ---------------------------------------------------------------------------

_env_cache: dict[str, dict] = {}


async def _get_env_info(env_id: str) -> dict:
    """Look up an environment's fields from Temper, with caching."""
    if env_id in _env_cache:
        return _env_cache[env_id]
    async with httpx.AsyncClient() as client:
        resp = await client.get(
            f"{TEMPER_API_URL}/tdata/Environments('{env_id}')",
            headers={"X-Tenant-Id": TEMPER_TENANT},
        )
    if resp.status_code != 200:
        raise HTTPException(
            status_code=400,
            detail=f"Failed to fetch environment {env_id}: HTTP {resp.status_code}",
        )
    body = resp.json()
    fields = body.get("fields", {})
    _env_cache[env_id] = fields
    return fields


# ---------------------------------------------------------------------------
# Request / Response models
# ---------------------------------------------------------------------------


class ToolRequest(BaseModel):
    arguments: dict = {}
    environment_id: Optional[str] = None


class ToolResponse(BaseModel):
    output: str
    is_error: bool = False




# ---------------------------------------------------------------------------
# Local tool implementations
# ---------------------------------------------------------------------------


def _truncate(s: str) -> str:
    if len(s) > MAX_OUTPUT_BYTES:
        return s[:MAX_OUTPUT_BYTES] + "\n... (output truncated)"
    return s


def _local_bash(args: dict) -> ToolResponse:
    command = args.get("command", "")
    if not command:
        return ToolResponse(output="bash: missing required parameter `command`", is_error=True)
    try:
        result = subprocess.run(
            ["bash", "-c", command],
            capture_output=True,
            text=True,
            timeout=120,
        )
        output = result.stdout
        if result.stderr:
            if output:
                output += "\n"
            output += "[stderr]\n" + result.stderr
        return ToolResponse(output=_truncate(output), is_error=result.returncode != 0)
    except subprocess.TimeoutExpired:
        return ToolResponse(output="bash: command timed out after 120s", is_error=True)
    except Exception as e:
        return ToolResponse(output=f"bash: failed to execute: {e}", is_error=True)


def _local_read(args: dict) -> ToolResponse:
    file_path = args.get("file_path", "")
    if not file_path:
        return ToolResponse(output="read: missing required parameter `file_path`", is_error=True)
    try:
        content = Path(file_path).read_text()
    except Exception as e:
        return ToolResponse(output=f"read: {file_path}: {e}", is_error=True)
    lines = content.splitlines()
    offset = int(args.get("offset", 0))
    limit = args.get("limit")
    end = (offset + int(limit)) if limit is not None else len(lines)
    end = min(end, len(lines))
    out = ""
    for i in range(offset, end):
        out += f"{i + 1}\t{lines[i]}\n"
    return ToolResponse(output=_truncate(out))


def _local_write(args: dict) -> ToolResponse:
    file_path = args.get("file_path", "")
    content = args.get("content", "")
    if not file_path:
        return ToolResponse(output="write: missing required parameter `file_path`", is_error=True)
    if "content" not in args:
        return ToolResponse(output="write: missing required parameter `content`", is_error=True)
    try:
        p = Path(file_path)
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content)
        return ToolResponse(output=f"wrote {len(content)} bytes to {file_path}")
    except Exception as e:
        return ToolResponse(output=f"write: {file_path}: {e}", is_error=True)


def _local_edit(args: dict) -> ToolResponse:
    file_path = args.get("file_path", "")
    old_string = args.get("old_string", "")
    new_string = args.get("new_string", "")
    if not file_path:
        return ToolResponse(output="edit: missing required parameter `file_path`", is_error=True)
    if "old_string" not in args:
        return ToolResponse(output="edit: missing required parameter `old_string`", is_error=True)
    if "new_string" not in args:
        return ToolResponse(output="edit: missing required parameter `new_string`", is_error=True)
    try:
        content = Path(file_path).read_text()
    except Exception as e:
        return ToolResponse(output=f"edit: {file_path}: {e}", is_error=True)
    count = content.count(old_string)
    if count == 0:
        return ToolResponse(output=f"edit: `old_string` not found in {file_path}", is_error=True)
    if count > 1:
        return ToolResponse(
            output=f"edit: `old_string` found {count} times in {file_path} — must be unique",
            is_error=True,
        )
    new_content = content.replace(old_string, new_string, 1)
    try:
        Path(file_path).write_text(new_content)
        return ToolResponse(output=f"edited {file_path}")
    except Exception as e:
        return ToolResponse(output=f"edit: failed to write {file_path}: {e}", is_error=True)


def _local_glob(args: dict) -> ToolResponse:
    pattern = args.get("pattern", "")
    if not pattern:
        return ToolResponse(output="glob: missing required parameter `pattern`", is_error=True)
    base = args.get("path", ".")
    # Use find via subprocess for recursive glob (matches Rust behavior)
    try:
        result = subprocess.run(
            ["bash", "-c", f"find {shlex.quote(base)} -name {shlex.quote(pattern)} 2>/dev/null | head -{MAX_GLOB_RESULTS}"],
            capture_output=True,
            text=True,
            timeout=30,
        )
        output = result.stdout.strip()
        if not output:
            return ToolResponse(output="(no matches)\n")
        return ToolResponse(output=_truncate(output + "\n"))
    except Exception as e:
        return ToolResponse(output=f"glob: {e}", is_error=True)


def _local_grep(args: dict) -> ToolResponse:
    pattern = args.get("pattern", "")
    if not pattern:
        return ToolResponse(output="grep: missing required parameter `pattern`", is_error=True)
    path = args.get("path", ".")
    include = args.get("include")
    cmd = ["grep", "-rn", pattern, path]
    if include:
        cmd.extend(["--include", include])
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
        if result.returncode == 1 and not result.stdout:
            return ToolResponse(output="(no matches)\n")
        return ToolResponse(output=_truncate(result.stdout))
    except Exception as e:
        return ToolResponse(output=f"grep: failed to execute: {e}", is_error=True)


_LOCAL_TOOLS = {
    "bash": _local_bash,
    "read": _local_read,
    "write": _local_write,
    "edit": _local_edit,
    "glob": _local_glob,
    "grep": _local_grep,
}


# ---------------------------------------------------------------------------
# Modal tool implementations
# ---------------------------------------------------------------------------


def _modal_exec(sandbox_id: str, command: str) -> ToolResponse:
    sb = _get_sandbox(sandbox_id)
    try:
        process = sb.exec("bash", "-c", command, timeout=120)
        stdout = process.stdout.read()
        stderr = process.stderr.read()
        exit_code = process.wait()
    except Exception as e:
        return ToolResponse(output=f"Modal exec failed: {e}", is_error=True)
    output = _truncate(stdout)
    if stderr:
        if output:
            output += "\n"
        output += stderr
    return ToolResponse(output=output, is_error=exit_code != 0)


def _modal_read(sandbox_id: str, args: dict) -> ToolResponse:
    file_path = args.get("file_path", "")
    if not file_path:
        return ToolResponse(output="read: missing required parameter `file_path`", is_error=True)
    sb = _get_sandbox(sandbox_id)
    process = sb.exec("cat", "-n", file_path)
    content = process.stdout.read()
    stderr = process.stderr.read()
    exit_code = process.wait()
    if exit_code != 0:
        return ToolResponse(output=stderr or f"Failed to read {file_path}", is_error=True)
    lines = content.splitlines()
    offset = int(args.get("offset", 0))
    limit = args.get("limit")
    if offset:
        lines = lines[offset:]
    if limit is not None:
        lines = lines[: int(limit)]
    return ToolResponse(output="\n".join(lines))


def _modal_write(sandbox_id: str, args: dict) -> ToolResponse:
    file_path = args.get("file_path", "")
    content = args.get("content", "")
    if not file_path:
        return ToolResponse(output="write: missing required parameter `file_path`", is_error=True)
    sb = _get_sandbox(sandbox_id)
    escaped_path = shlex.quote(file_path)
    process = sb.exec(
        "bash",
        "-c",
        f"mkdir -p $(dirname {escaped_path}) && cat > {escaped_path}",
    )
    process.stdin.write(content)
    process.stdin.write_eof()
    process.stdin.drain()
    exit_code = process.wait()
    if exit_code != 0:
        stderr = process.stderr.read()
        return ToolResponse(output=f"write failed: {stderr}", is_error=True)
    return ToolResponse(output=f"wrote to {file_path}")


def _modal_read_raw(sandbox_id: str, file_path: str) -> tuple[str, ToolResponse | None]:
    """Read raw file content (no line numbers) for internal use by edit."""
    sb = _get_sandbox(sandbox_id)
    process = sb.exec("cat", file_path)
    content = process.stdout.read()
    stderr = process.stderr.read()
    exit_code = process.wait()
    if exit_code != 0:
        return "", ToolResponse(output=stderr or f"Failed to read {file_path}", is_error=True)
    return content, None


def _modal_edit(sandbox_id: str, args: dict) -> ToolResponse:
    file_path = args.get("file_path", "")
    old_string = args.get("old_string", "")
    new_string = args.get("new_string", "")
    if not file_path:
        return ToolResponse(output="edit: missing required parameter `file_path`", is_error=True)
    # Read raw content (no line numbers)
    content, err = _modal_read_raw(sandbox_id, file_path)
    if err:
        return err
    count = content.count(old_string)
    if count == 0:
        return ToolResponse(output=f"edit: old_string not found in {file_path}", is_error=True)
    if count > 1:
        return ToolResponse(
            output=f"edit: old_string found {count} times in {file_path} (must be unique)",
            is_error=True,
        )
    new_content = content.replace(old_string, new_string, 1)
    return _modal_write(sandbox_id, {"file_path": file_path, "content": new_content})


def _modal_tool(sandbox_id: str, tool_name: str, args: dict) -> ToolResponse:
    if tool_name == "bash":
        command = args.get("command", "")
        if not command:
            return ToolResponse(output="bash: missing required parameter `command`", is_error=True)
        return _modal_exec(sandbox_id, command)
    elif tool_name == "read":
        return _modal_read(sandbox_id, args)
    elif tool_name == "write":
        return _modal_write(sandbox_id, args)
    elif tool_name == "edit":
        return _modal_edit(sandbox_id, args)
    elif tool_name == "glob":
        pattern = args.get("pattern", "*")
        path = args.get("path", ".")
        cmd = f"find {shlex.quote(path)} -name {shlex.quote(pattern)} 2>/dev/null | head -1000"
        return _modal_exec(sandbox_id, cmd)
    elif tool_name == "grep":
        pattern = args.get("pattern", "")
        path = args.get("path", ".")
        include = args.get("include")
        cmd = f"grep -rn {shlex.quote(pattern)}"
        if include:
            cmd += f" --include={shlex.quote(include)}"
        cmd += f" {shlex.quote(path)}"
        return _modal_exec(sandbox_id, cmd)
    else:
        return ToolResponse(output=f"unknown tool: {tool_name}", is_error=True)


# ---------------------------------------------------------------------------
# Lazy sandbox provisioning
# ---------------------------------------------------------------------------


async def _provision_sandbox(env_id: str, env_fields: dict) -> str:
    """Create a Modal sandbox for the given environment and cache the id."""
    modal = _ensure_modal()

    image_name = env_fields.get("ModalImage")
    image = modal.Image.from_registry(image_name) if image_name else modal.Image.debian_slim()

    sb = modal.Sandbox.create(
        "sleep",
        "infinity",
        app=_get_modal_app(),
        image=image,
        cpu=env_fields.get("ModalCpu", 1.0),
        memory=env_fields.get("ModalMemory", 2048),
        timeout=env_fields.get("ModalTimeout", 600),
        gpu=env_fields.get("ModalGpu"),
        block_network=env_fields.get("ModalBlockNetwork", False),
        workdir=env_fields.get("ModalWorkdir", "/workspace"),
        idle_timeout=300,
    )

    sandbox_id = sb.object_id
    _sandboxes[sandbox_id] = sb
    logger.info("Lazily provisioned sandbox %s for environment %s", sandbox_id, env_id)

    # Update cache and persist to Temper
    _env_cache[env_id] = {**env_fields, "SandboxId": sandbox_id}
    async with httpx.AsyncClient() as client:
        await client.patch(
            f"{TEMPER_API_URL}/tdata/Environments('{env_id}')",
            headers={"X-Tenant-Id": TEMPER_TENANT, "Content-Type": "application/json"},
            json={"SandboxId": sandbox_id},
        )

    return sandbox_id


# ---------------------------------------------------------------------------
# Handlers
# ---------------------------------------------------------------------------


@app.get("/health")
async def health():
    return {"status": "ok", "service": "crucible-tool-server"}


@app.post("/tools/{tool_name}", response_model=ToolResponse)
async def handle_tool(tool_name: str, body: ToolRequest):
    """Execute a tool, routing to Local or Modal based on environment ConfigType.
    Memory tools always go through Temper OData (no sandbox needed)."""

    # Memory + system tools bypass Local/Modal routing — they talk to Temper directly.
    if tool_name.startswith("memory_"):
        return await _handle_memory_tool(tool_name, body.arguments)
    if tool_name == "trigger_session":
        return await _trigger_session(body.arguments)

    config_type = "Local"
    sandbox_id = None

    if body.environment_id:
        try:
            env_fields = await _get_env_info(body.environment_id)
            config_type = env_fields.get("ConfigType", "Local")
            sandbox_id = env_fields.get("SandboxId")
        except HTTPException:
            raise
        except Exception as e:
            return ToolResponse(output=f"failed to look up environment: {e}", is_error=True)

    if config_type == "Modal":
        if not sandbox_id:
            # Lazy provisioning: create sandbox on first tool call.
            try:
                sandbox_id = await _provision_sandbox(body.environment_id, env_fields)
            except Exception as e:
                return ToolResponse(output=f"failed to provision Modal sandbox: {e}", is_error=True)
        return _modal_tool(sandbox_id, tool_name, body.arguments)
    else:
        # Local (or any unknown ConfigType falls back to local)
        handler = _LOCAL_TOOLS.get(tool_name)
        if handler is None:
            return ToolResponse(output=f"unknown tool: {tool_name}", is_error=True)
        return handler(body.arguments)


# ---------------------------------------------------------------------------
# Memory tools — always route through Temper OData, never a sandbox
# ---------------------------------------------------------------------------


async def _handle_memory_tool(tool_name: str, args: dict) -> ToolResponse:
    """Execute a memory tool against Temper's OData API."""
    try:
        if tool_name == "memory_list":
            return await _memory_list(args)
        elif tool_name == "memory_read":
            return await _memory_read(args)
        elif tool_name == "memory_write":
            return await _memory_write(args)
        elif tool_name == "memory_search":
            return await _memory_search(args)
        else:
            return ToolResponse(output=f"unknown memory tool: {tool_name}", is_error=True)
    except Exception as e:
        return ToolResponse(output=f"{tool_name} error: {e}", is_error=True)


async def _memory_list(args: dict) -> ToolResponse:
    store_id = args.get("memory_store_id", "")
    path_prefix = args.get("path_prefix", "")
    async with httpx.AsyncClient() as client:
        resp = await client.get(
            f"{TEMPER_API_URL}/tdata/Memories",
            params={"$filter": f"MemoryStoreId eq '{store_id}'"},
            headers={"X-Tenant-Id": TEMPER_TENANT},
        )
    if resp.status_code != 200:
        return ToolResponse(output=f"memory_list failed: HTTP {resp.status_code}", is_error=True)
    data = resp.json()
    memories = data.get("value", [])
    lines = []
    for m in memories:
        f = m.get("fields", {})
        path = f.get("Path", "")
        if path_prefix and not path.startswith(path_prefix):
            continue
        size = f.get("SizeBytes", 0) or 0
        mid = m.get("entity_id", f.get("Id", ""))
        lines.append(f"- {path} ({size} bytes) [id: {mid}]")
    if not lines:
        return ToolResponse(output="No memories found.")
    return ToolResponse(output=f"{len(lines)} memories:\n" + "\n".join(lines))


async def _memory_read(args: dict) -> ToolResponse:
    mem_id = args.get("memory_id", "")
    async with httpx.AsyncClient() as client:
        resp = await client.get(
            f"{TEMPER_API_URL}/tdata/Memories('{mem_id}')",
            headers={"X-Tenant-Id": TEMPER_TENANT},
        )
    if resp.status_code != 200:
        return ToolResponse(output=f"memory_read failed: HTTP {resp.status_code}", is_error=True)
    data = resp.json()
    f = data.get("fields", {})
    path = f.get("Path", "?")
    content = f.get("Content", "")
    return ToolResponse(output=f"Path: {path}\nContent:\n{content}")


async def _memory_write(args: dict) -> ToolResponse:
    store_id = args.get("memory_store_id", "")
    path = args.get("path", "")
    content = args.get("content", "")
    size = len(content)
    now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"

    # Check if a memory at this path already exists
    async with httpx.AsyncClient() as client:
        resp = await client.get(
            f"{TEMPER_API_URL}/tdata/Memories",
            params={"$filter": f"MemoryStoreId eq '{store_id}'"},
            headers={"X-Tenant-Id": TEMPER_TENANT},
        )
    existing = None
    if resp.status_code == 200:
        for m in resp.json().get("value", []):
            if m.get("fields", {}).get("Path") == path:
                existing = m
                break

    async with httpx.AsyncClient() as client:
        if existing:
            # Update existing memory
            mem_id = existing.get("entity_id", existing.get("fields", {}).get("Id", ""))
            resp = await client.patch(
                f"{TEMPER_API_URL}/tdata/Memories('{mem_id}')",
                headers={"X-Tenant-Id": TEMPER_TENANT, "Content-Type": "application/json"},
                json={"Content": content, "SizeBytes": size, "UpdatedAt": now},
            )
            if resp.status_code >= 400:
                return ToolResponse(output=f"memory_write update failed: HTTP {resp.status_code} {resp.text[:200]}", is_error=True)
            return ToolResponse(output=f"Updated memory at {path} ({size} bytes)")
        else:
            # Create new memory
            slug = path.strip("/").replace("/", "-")
            mem_id = f"mem-{slug}"
            resp = await client.post(
                f"{TEMPER_API_URL}/tdata/Memories",
                headers={"X-Tenant-Id": TEMPER_TENANT, "Content-Type": "application/json"},
                json={
                    "id": mem_id,
                    "MemoryStoreId": store_id,
                    "Path": path,
                    "Content": content,
                    "SizeBytes": size,
                    "CreatedAt": now,
                    "UpdatedAt": now,
                },
            )
            if resp.status_code >= 400:
                return ToolResponse(output=f"memory_write create failed: HTTP {resp.status_code} {resp.text[:200]}", is_error=True)
            return ToolResponse(output=f"Created memory at {path} ({size} bytes)")


async def _memory_search(args: dict) -> ToolResponse:
    store_id = args.get("memory_store_id", "")
    query = args.get("query", "").lower()

    # List all memories, then read each to search content
    async with httpx.AsyncClient() as client:
        resp = await client.get(
            f"{TEMPER_API_URL}/tdata/Memories",
            params={"$filter": f"MemoryStoreId eq '{store_id}'"},
            headers={"X-Tenant-Id": TEMPER_TENANT},
        )
    if resp.status_code != 200:
        return ToolResponse(output=f"memory_search failed: HTTP {resp.status_code}", is_error=True)

    results = []
    for m in resp.json().get("value", []):
        f = m.get("fields", {})
        mid = m.get("entity_id", f.get("Id", ""))
        path = f.get("Path", "")
        # Read full content
        async with httpx.AsyncClient() as client:
            r2 = await client.get(
                f"{TEMPER_API_URL}/tdata/Memories('{mid}')",
                headers={"X-Tenant-Id": TEMPER_TENANT},
            )
        if r2.status_code == 200:
            content = r2.json().get("fields", {}).get("Content", "")
            if query in content.lower():
                preview = content[:200] + ("..." if len(content) > 200 else "")
                results.append(f"- {path} [id: {mid}]\n  {preview}")

    if not results:
        return ToolResponse(output=f"No memories matching '{query}' found.")
    return ToolResponse(output=f"{len(results)} matches for '{query}':\n\n" + "\n\n".join(results))


async def _trigger_session(args: dict) -> ToolResponse:
    """Post a user.message to any session, triggering the agent loop."""
    session_id = args.get("session_id", "")
    message_text = args.get("message", "")
    if not session_id or not message_text:
        return ToolResponse(output="trigger_session requires session_id and message", is_error=True)

    now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"

    # Get next sequence
    async with httpx.AsyncClient() as c:
        resp = await c.get(
            f"{TEMPER_API_URL}/tdata/SessionEvents",
            params={"$filter": f"SessionId eq '{session_id}'", "$orderby": "Sequence desc", "$top": "1"},
            headers={"X-Tenant-Id": TEMPER_TENANT},
        )
    max_seq = -1
    if resp.status_code == 200:
        values = resp.json().get("value", [])
        if values:
            max_seq = values[0]["fields"].get("Sequence", -1)
    seq = max_seq + 1

    # Post user.message
    event = {
        "id": f"ev-trigger-{session_id}-{seq}",
        "SessionId": session_id,
        "Sequence": seq,
        "Kind": "user.message",
        "Content": json.dumps({"blocks": [{"type": "text", "text": message_text}]}),
        "CreatedAt": now,
        "ProcessedAt": now,
    }
    async with httpx.AsyncClient() as c:
        resp = await c.post(
            f"{TEMPER_API_URL}/tdata/SessionEvents",
            headers={"X-Tenant-Id": TEMPER_TENANT, "Content-Type": "application/json"},
            json=event,
        )
    if resp.status_code >= 400:
        return ToolResponse(output=f"trigger_session failed: HTTP {resp.status_code}", is_error=True)
    return ToolResponse(output=f"Triggered session {session_id} with message (seq={seq}). The agent will process it shortly.")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    import uvicorn

    port = int(os.environ.get("PORT", "3100"))
    logger.info("Starting Crucible Tool Server on port %d", port)
    uvicorn.run(app, host="127.0.0.1", port=port)
