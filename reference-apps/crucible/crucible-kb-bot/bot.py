"""Crucible Knowledge Base Discord Bot.

Bridges Discord channels to Crucible sessions:
  #ingest  — drop PDFs, links, or text. Bot processes and confirms.
  #ask     — ask questions against the knowledge base.
  #admin   — manage schedules, check stats, run lints.

Environment variables:
  DISCORD_BOT_TOKEN        — bot token
  TEMPER_URL               — temper serve URL (default: http://127.0.0.1:3000)
  TEMPER_TENANT            — tenant name (default: crucible)
  INGEST_CHANNEL_ID        — Discord channel ID for #ingest
  ASK_CHANNEL_ID           — Discord channel ID for #ask
  ADMIN_CHANNEL_ID         — Discord channel ID for #admin
  INGEST_SESSION_ID        — Crucible session ID for the ingest agent
  QA_SESSION_ID            — Crucible session ID for the Q&A agent
"""

import asyncio
import json
import logging
import os
import time
from datetime import datetime, timezone
from typing import Optional

import discord
import httpx

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s")
logger = logging.getLogger("crucible-kb-bot")

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

TEMPER_URL = os.environ.get("TEMPER_URL", "http://127.0.0.1:3000")
TEMPER_TENANT = os.environ.get("TEMPER_TENANT", "crucible")

INGEST_CHANNEL_ID = int(os.environ.get("INGEST_CHANNEL_ID", "0"))
ASK_CHANNEL_ID = int(os.environ.get("ASK_CHANNEL_ID", "0"))
ADMIN_CHANNEL_ID = int(os.environ.get("ADMIN_CHANNEL_ID", "0"))
DEBUG_CHANNEL_ID = int(os.environ.get("DEBUG_CHANNEL_ID", "0"))

INGEST_SESSION_ID = os.environ.get("INGEST_SESSION_ID", "")
QA_SESSION_ID = os.environ.get("QA_SESSION_ID", "")
LINT_SESSION_ID = os.environ.get("LINT_SESSION_ID", "")
ADMIN_SESSION_ID = os.environ.get("ADMIN_SESSION_ID", "")

HEADERS = {"X-Tenant-Id": TEMPER_TENANT, "Content-Type": "application/json"}

# ---------------------------------------------------------------------------
# Temper OData helpers
# ---------------------------------------------------------------------------

http = httpx.AsyncClient(timeout=120)


def now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


async def get_next_sequence(session_id: str) -> int:
    """Get the max sequence in the event feed + 1."""
    resp = await http.get(
        f"{TEMPER_URL}/tdata/SessionEvents",
        params={
            "$filter": f"SessionId eq '{session_id}'",
            "$orderby": "Sequence desc",
            "$top": "1",
        },
        headers={"X-Tenant-Id": TEMPER_TENANT},
    )
    data = resp.json()
    values = data.get("value", [])
    if values:
        return values[0]["fields"]["Sequence"] + 1
    return 0


async def post_user_message(session_id: str, text: str) -> int:
    """POST a user.message event. Returns the sequence number."""
    seq = await get_next_sequence(session_id)
    now = now_iso()
    body = {
        "id": f"ev-discord-{session_id}-{seq}",
        "SessionId": session_id,
        "Sequence": seq,
        "Kind": "user.message",
        "Content": json.dumps({"blocks": [{"type": "text", "text": text}]}),
        "CreatedAt": now,
        "ProcessedAt": now,
    }
    resp = await http.post(
        f"{TEMPER_URL}/tdata/SessionEvents", headers=HEADERS, json=body
    )
    if resp.status_code >= 400:
        logger.error("POST event failed: %s %s", resp.status_code, resp.text[:300])
    return seq


async def poll_for_response(session_id: str, after_seq: int, timeout: int = 90) -> Optional[str]:
    """Poll for an agent.message event that didn't exist when we started.

    Uses the latest agent.message by descending sequence, and checks
    that it appeared after we started polling (by comparing against
    after_seq OR by waiting for session.status_idle which means the
    turn is done).
    """
    start_time = time.time()
    deadline = start_time + timeout

    while time.time() < deadline:
        # Check if session went to Idle (turn complete)
        try:
            sess_resp = await http.get(
                f"{TEMPER_URL}/tdata/Sessions('{session_id}')",
                headers={"X-Tenant-Id": TEMPER_TENANT},
            )
            sess_data = sess_resp.json()
            status = sess_data.get("status", "")
        except Exception:
            status = ""

        # If session is Idle and we've waited at least 3 seconds
        # (to avoid catching stale Idle from before our message),
        # grab the latest agent.message
        if status == "Idle" and (time.time() - start_time) > 3:
            resp = await http.get(
                f"{TEMPER_URL}/tdata/SessionEvents",
                params={
                    "$filter": f"SessionId eq '{session_id}' and Kind eq 'agent.message'",
                    "$orderby": "Sequence desc",
                    "$top": "1",
                },
                headers={"X-Tenant-Id": TEMPER_TENANT},
            )
            data = resp.json()
            values = data.get("value", [])
            if values:
                content = json.loads(values[0]["fields"].get("Content", "{}"))
                blocks = content.get("blocks", [])
                if blocks:
                    return blocks[0].get("text", "")

        await asyncio.sleep(2)
    return None


async def get_session_status(session_id: str) -> str:
    """Get the current session status."""
    resp = await http.get(
        f"{TEMPER_URL}/tdata/Sessions('{session_id}')",
        headers={"X-Tenant-Id": TEMPER_TENANT},
    )
    if resp.status_code != 200:
        return "unknown"
    return resp.json().get("status", "unknown")


async def list_memories(store_id: str, path_prefix: str = "/") -> list[dict]:
    """List memories in a store, optionally filtered by path prefix."""
    resp = await http.get(
        f"{TEMPER_URL}/tdata/Memories",
        params={"$filter": f"MemoryStoreId eq '{store_id}'"},
        headers={"X-Tenant-Id": TEMPER_TENANT},
    )
    data = resp.json()
    memories = []
    for item in data.get("value", []):
        fields = item.get("fields", {})
        path = fields.get("Path", "")
        if path.startswith(path_prefix):
            memories.append(fields)
    return memories


# ---------------------------------------------------------------------------
# Discord bot
# ---------------------------------------------------------------------------

intents = discord.Intents.default()
intents.message_content = True
client = discord.Client(intents=intents)


def truncate(text: str, max_len: int = 1900) -> str:
    """Truncate to fit Discord's 2000 char limit with room for formatting."""
    if len(text) <= max_len:
        return text
    return text[:max_len] + "\n\n… *(truncated)*"


@client.event
async def on_ready():
    logger.info("Bot connected as %s", client.user)
    logger.info("Ingest channel: %s", INGEST_CHANNEL_ID)
    logger.info("Ask channel: %s", ASK_CHANNEL_ID)
    logger.info("Admin channel: %s", ADMIN_CHANNEL_ID)
    logger.info("Debug channel: %s", DEBUG_CHANNEL_ID)

    # Start the debug event poller
    if DEBUG_CHANNEL_ID:
        client.loop.create_task(debug_event_poller())


@client.event
async def on_message(message: discord.Message):
    if message.author.bot:
        return

    channel_id = message.channel.id

    if channel_id == INGEST_CHANNEL_ID:
        await handle_ingest(message)
    elif channel_id == ASK_CHANNEL_ID:
        await handle_ask(message)
    elif channel_id == ADMIN_CHANNEL_ID:
        await handle_admin(message)
    elif channel_id == DEBUG_CHANNEL_ID:
        await handle_debug(message)


async def handle_ingest(message: discord.Message):
    """Process a document dropped in #ingest."""
    parts = []
    if message.content:
        parts.append(message.content)
    for attachment in message.attachments:
        parts.append(f"Process this document: {attachment.url} (filename: {attachment.filename})")
    for embed in message.embeds:
        if embed.url:
            parts.append(f"Process this link: {embed.url}")
    if not parts:
        await message.reply("Send a PDF, link, or text to ingest.")
        return

    text = "\n".join(parts)
    await message.add_reaction("⏳")

    try:
        seq = await post_user_message(INGEST_SESSION_ID, text)
        logger.info("Ingest: posted user.message seq=%d", seq)
        # Fire and forget — poll in background, reply when ready
        client.loop.create_task(
            _wait_and_reply(message, INGEST_SESSION_ID, seq, "✅ ", "⏳")
        )
    except Exception as e:
        logger.error("Ingest error: %s", e)
        await message.reply(f"❌ Error: {e}")


async def handle_ask(message: discord.Message):
    """Answer a question from #ask."""
    if not message.content:
        return

    await message.add_reaction("🔍")

    try:
        seq = await post_user_message(QA_SESSION_ID, message.content)
        logger.info("Q&A: posted user.message seq=%d", seq)
        # Fire and forget — poll in background, reply when ready
        client.loop.create_task(
            _wait_and_reply(message, QA_SESSION_ID, seq, "", "🔍")
        )
    except Exception as e:
        logger.error("Q&A error: %s", e)
        await message.reply(f"❌ Error: {e}")


async def _wait_and_reply(
    message: discord.Message,
    session_id: str,
    after_seq: int,
    prefix: str,
    reaction: str,
    timeout: int = 600,
):
    """Poll for agent response and reply when it arrives. Runs as background task."""
    response = await poll_for_response(session_id, after_seq, timeout=timeout)
    try:
        if response:
            await message.reply(truncate(f"{prefix}{response}"))
        else:
            await message.reply("⏰ Agent didn't respond within 10 minutes.")
        await message.remove_reaction(reaction, client.user)
    except Exception as e:
        logger.warning("Failed to reply: %s", e)


async def handle_admin(message: discord.Message):
    """Route admin messages through the admin agent."""
    if not message.content:
        return

    await message.add_reaction("⚙️")

    session_id = ADMIN_SESSION_ID if ADMIN_SESSION_ID else QA_SESSION_ID
    try:
        seq = await post_user_message(session_id, message.content)
        logger.info("Admin: posted user.message seq=%d to %s", seq, session_id)
        client.loop.create_task(
            _wait_and_reply(message, session_id, seq, "", "⚙️")
        )
    except Exception as e:
        logger.error("Admin error: %s", e)
        await message.reply(f"❌ Error: {e}")


## admin_stats and admin_lint removed — #admin is now fully agentic


async def handle_debug(message: discord.Message):
    """Answer questions about the system state in #debug."""
    text = message.content.strip().lower()

    if text in ("agents", "list agents"):
        resp = await http.get(
            f"{TEMPER_URL}/tdata/ManagedAgents",
            headers={"X-Tenant-Id": TEMPER_TENANT},
        )
        agents = resp.json().get("value", [])
        lines = ["**Agents:**"]
        for a in agents:
            f = a.get("fields", {})
            lines.append(
                f"• `{a['entity_id']}` — **{f.get('Name','?')}** "
                f"(model=`{f.get('ModelId','?')}`, status={a['status']})"
            )
        await message.reply("\n".join(lines) or "No agents found.")

    elif text in ("sessions", "list sessions"):
        resp = await http.get(
            f"{TEMPER_URL}/tdata/Sessions",
            headers={"X-Tenant-Id": TEMPER_TENANT},
        )
        sessions = resp.json().get("value", [])
        lines = ["**Sessions:**"]
        for s in sessions:
            f = s.get("fields", {})
            lines.append(
                f"• `{s['entity_id']}` — status=**{s['status']}** "
                f"agent=`{f.get('AgentId','?')}` env=`{f.get('EnvironmentId','?')}`"
            )
        await message.reply("\n".join(lines) or "No sessions found.")

    elif text in ("envs", "environments", "list environments"):
        resp = await http.get(
            f"{TEMPER_URL}/tdata/Environments",
            headers={"X-Tenant-Id": TEMPER_TENANT},
        )
        envs = resp.json().get("value", [])
        lines = ["**Environments:**"]
        for e in envs:
            f = e.get("fields", {})
            config = f.get("ConfigType", "?")
            extra = ""
            if config == "Modal":
                extra = f" image=`{f.get('ModalImage','?')}` cpu={f.get('ModalCpu','?')} mem={f.get('ModalMemory','?')}MB"
            lines.append(
                f"• `{e['entity_id']}` — **{f.get('Name','?')}** "
                f"type=`{config}`{extra} status={e['status']}"
            )
        await message.reply("\n".join(lines) or "No environments found.")

    elif text in ("memories", "memory", "kb", "knowledge base"):
        resp = await http.get(
            f"{TEMPER_URL}/tdata/MemoryStores",
            headers={"X-Tenant-Id": TEMPER_TENANT},
        )
        stores = resp.json().get("value", [])
        if not stores:
            await message.reply("No memory stores found.")
            return
        for store in stores:
            store_id = store["entity_id"]
            f = store.get("fields", {})
            memories = await list_memories(store_id)
            lines = [f"**Memory Store: {f.get('Name','?')}** (`{store_id}`, {len(memories)} memories)"]
            for m in memories[:20]:
                size = m.get("SizeBytes", 0) or 0
                lines.append(f"  `{m.get('Path','?')}` ({size} bytes)")
            if len(memories) > 20:
                lines.append(f"  … and {len(memories) - 20} more")
            await message.reply("\n".join(lines))

    elif text in ("schedules", "cron", "list schedules"):
        resp = await http.get(
            f"{TEMPER_URL}/tdata/SessionSchedules",
            headers={"X-Tenant-Id": TEMPER_TENANT},
        )
        scheds = resp.json().get("value", [])
        if not scheds:
            await message.reply("No schedules found.")
            return
        lines = ["**Schedules:**"]
        for s in scheds:
            f = s.get("fields", {})
            lines.append(
                f"• `{s['entity_id']}` — status=**{s['status']}** "
                f"cron=`{f.get('CronExpression','?')}` "
                f"session=`{f.get('SessionId','?')}`"
            )
        await message.reply("\n".join(lines))

    elif text.startswith("events "):
        # "events sess-ingest" or "events sess-qa 10"
        parts = text.split()
        sess_id = parts[1] if len(parts) > 1 else ""
        limit = int(parts[2]) if len(parts) > 2 else 5
        resp = await http.get(
            f"{TEMPER_URL}/tdata/SessionEvents",
            params={
                "$filter": f"SessionId eq '{sess_id}'",
                "$orderby": "Sequence desc",
                "$top": str(limit),
            },
            headers={"X-Tenant-Id": TEMPER_TENANT},
        )
        events = resp.json().get("value", [])
        events.reverse()
        lines = [f"**Last {len(events)} events for `{sess_id}`:**"]
        for e in events:
            f = e.get("fields", {})
            lines.append(f"  seq={f.get('Sequence',0):>3} `{f.get('Kind','?')}`")
        await message.reply("\n".join(lines) or f"No events for `{sess_id}`.")

    elif text in ("tools", "list tools"):
        resp = await http.get(
            f"{TEMPER_URL}/tdata/AgentTools",
            headers={"X-Tenant-Id": TEMPER_TENANT},
        )
        tools = resp.json().get("value", [])
        lines = ["**Agent Tools:**"]
        for t in tools:
            f = t.get("fields", {})
            lines.append(
                f"• `{t['entity_id']}` — agent=`{f.get('AgentId','?')}` kind=`{f.get('Kind','?')}`"
            )
        await message.reply("\n".join(lines) or "No tools found.")

    elif text in ("help", "commands"):
        await message.reply(
            "**Debug commands:**\n"
            "• `agents` — list all agents\n"
            "• `sessions` — list all sessions with status\n"
            "• `environments` — list environments\n"
            "• `memories` — list memory stores and their contents\n"
            "• `schedules` — list cron schedules\n"
            "• `tools` — list agent tools\n"
            "• `events <session-id> [count]` — last N events for a session\n"
            "• `stats` — knowledge base statistics\n"
            "• `help` — this message"
        )

    else:
        # Unknown command — show help hint
        await message.reply("Unknown command. Type `help` for available commands.")


# ---------------------------------------------------------------------------
# Debug event poller — streams all session events to #debug channel
# ---------------------------------------------------------------------------

# Emoji mapping for event kinds
KIND_EMOJI = {
    "user.message": "💬",
    "user.interrupt": "🛑",
    "agent.message": "🤖",
    "agent.tool_use": "🔧",
    "agent.tool_result": "📋",
    "span.model_request_start": "⏱️",
    "span.model_request_end": "⏱️",
    "session.status_idle": "💤",
    "session.status_running": "🏃",
}

SESSION_LABELS = {}  # filled at startup


async def debug_event_poller():
    """Poll all session event feeds and post new events to #debug."""
    await client.wait_until_ready()

    debug_channel = client.get_channel(DEBUG_CHANNEL_ID)
    if not debug_channel:
        logger.warning("Debug channel %s not found", DEBUG_CHANNEL_ID)
        return

    # Track sessions and their last-seen sequence
    sessions = {}
    for sess_id, label in [
        (INGEST_SESSION_ID, "ingest"),
        (QA_SESSION_ID, "qa"),
        (LINT_SESSION_ID, "lint"),
    ]:
        if sess_id:
            # Get current max sequence
            seq = await get_next_sequence(sess_id)
            sessions[sess_id] = {"last_seq": seq - 1, "label": label}

    if not sessions:
        return

    await debug_channel.send("🔍 **Debug poller started.** Watching: " +
        ", ".join(f"`{v['label']}` ({k})" for k, v in sessions.items()))

    while not client.is_closed():
        try:
            for sess_id, info in sessions.items():
                resp = await http.get(
                    f"{TEMPER_URL}/tdata/SessionEvents",
                    params={
                        "$filter": f"SessionId eq '{sess_id}' and Sequence gt {info['last_seq']}",
                        "$orderby": "Sequence asc",
                        "$top": "20",
                    },
                    headers={"X-Tenant-Id": TEMPER_TENANT},
                )
                data = resp.json()
                events = data.get("value", [])

                for event in events:
                    fields = event.get("fields", {})
                    seq = fields.get("Sequence", 0)
                    kind = fields.get("Kind", "?")
                    emoji = KIND_EMOJI.get(kind, "📌")
                    label = info["label"]

                    # Build the debug message
                    parts = [f"{emoji} **{label}** seq={seq} `{kind}`"]

                    # Add relevant details based on kind
                    if kind in ("user.message", "agent.message"):
                        content = fields.get("Content", "")
                        try:
                            parsed = json.loads(content)
                            text = parsed.get("blocks", [{}])[0].get("text", "")
                            if text:
                                preview = text[:200] + ("…" if len(text) > 200 else "")
                                parts.append(f"```\n{preview}\n```")
                        except Exception:
                            pass

                    elif kind == "agent.tool_use":
                        tool_name = fields.get("ToolName", "?")
                        content = fields.get("Content", "")
                        try:
                            parsed = json.loads(content)
                            args = parsed.get("input", parsed.get("arguments", {}))
                            cmd = args.get("command", args.get("file_path", args.get("pattern", "")))
                            if cmd:
                                parts.append(f"tool=`{tool_name}` → `{str(cmd)[:150]}`")
                            else:
                                parts.append(f"tool=`{tool_name}`")
                        except Exception:
                            parts.append(f"tool=`{tool_name}`")

                    elif kind == "agent.tool_result":
                        content = fields.get("Content", "")
                        try:
                            parsed = json.loads(content)
                            output = parsed.get("output", "")
                            is_error = parsed.get("is_error", False)
                            status = "❌" if is_error else "✅"
                            preview = output[:150] + ("…" if len(output) > 150 else "")
                            parts.append(f"{status} `{preview}`")
                        except Exception:
                            pass

                    elif kind == "span.model_request_end":
                        tokens_in = fields.get("ModelInputTokens", "?")
                        tokens_out = fields.get("ModelOutputTokens", "?")
                        parts.append(f"in={tokens_in} out={tokens_out}")

                    elif kind == "session.status_idle":
                        reason = fields.get("StopReason", "?")
                        parts.append(f"stop_reason=`{reason}`")

                    msg = "\n".join(parts)
                    try:
                        await debug_channel.send(truncate(msg, 1900))
                    except Exception as e:
                        logger.warning("Failed to send debug msg: %s", e)

                    info["last_seq"] = seq

        except Exception as e:
            logger.warning("Debug poller error: %s", e)

        await asyncio.sleep(2)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    token = os.environ.get("DISCORD_BOT_TOKEN")
    if not token:
        logger.error("DISCORD_BOT_TOKEN not set")
        return

    if not INGEST_SESSION_ID or not QA_SESSION_ID:
        logger.error("INGEST_SESSION_ID and QA_SESSION_ID must be set")
        return

    if not INGEST_CHANNEL_ID or not ASK_CHANNEL_ID:
        logger.error("Channel IDs must be set")
        return

    logger.info("Starting Crucible KB bot...")
    client.run(token)


if __name__ == "__main__":
    main()
