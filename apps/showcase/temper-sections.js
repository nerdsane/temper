// Shared Temper integration sections for showcase apps
// Each app imports this and calls renderTemperSection(containerId, appConfig)

const TEMPER_SECTIONS = {
  'quick-form': {
    entity: 'Proposal',
    states: 'Seed → Planned → Approved → Implementing → Completed → Verified',
    spec: `# proposal.ioa.toml
[entity]
name = "Proposal"
initial_status = "Seed"

[vars]
is_selected = { type = "bool", init = false }
has_plan = { type = "bool", init = false }

[[actions]]
name = "Select"
from = ["Seed", "Planned", "Approved", "Implementing", "Completed"]
to = "@same"
effects = [{ set = { var = "is_selected", value = true } }]

[[actions]]
name = "WritePlan"
from = ["Seed"]
to = "Planned"
effects = [{ set = { var = "has_plan", value = true } }]

[[actions]]
name = "Approve"
from = ["Planned"]
to = "Approved"
guards = [{ is_true = "has_plan" }]

[[actions]]
name = "Implement"
from = ["Approved"]
to = "Implementing"

[[actions]]
name = "Complete"
from = ["Implementing"]
to = "Completed"

[[actions]]
name = "Verify"
from = ["Completed"]
to = "Verified"
# Verified is terminal — no actions out`,
    flow: `# Form submits → creates entity in Temper
POST /tdata/Proposals
X-Tenant-Id: haku-ops
{ "entity_id": "prop-029",
  "fields": { "Title": "Story embedding search",
              "Priority": "high", "Risk": "high",
              "Area": "backend" } }

# If auto-select was toggled on:
POST /tdata/Proposals('prop-029')/Temper.Select

# Haku's webhook fires → picks up the new seed
# Rita sees it in the dashboard, can Approve/Scratch`,
    why: 'The form creates a real Temper entity, not a JSON blob. The proposal goes through a verified state machine — you can\'t skip from Seed to Implementing, you can\'t Verify something that wasn\'t Completed. Every transition is logged in the trajectory table. Rita and Haku interact with the same state.'
  },

  'content-pipeline': {
    entity: 'Post',
    states: 'Draft → InReview → Scheduled → Published',
    spec: `# post.ioa.toml
[entity]
name = "Post"
initial_status = "Draft"

[vars]
has_review = { type = "bool", init = false }
is_scheduled = { type = "bool", init = false }

[[actions]]
name = "SubmitForReview"
from = ["Draft"]
to = "InReview"

[[actions]]
name = "Approve"
from = ["InReview"]
to = "Scheduled"
effects = [
  { set = { var = "has_review", value = true } },
  { set = { var = "is_scheduled", value = true } }
]

[[actions]]
name = "RequestChanges"
from = ["InReview"]
to = "Draft"

[[actions]]
name = "Publish"
from = ["Scheduled"]
to = "Published"
guards = [{ is_true = "has_review" }]

[[actions]]
name = "Unpublish"
from = ["Published"]
to = "Draft"
effects = [
  { set = { var = "is_scheduled", value = false } }
]`,
    flow: `# Jiji writes a blog draft
POST /tdata/Posts
X-Tenant-Id: calcifer-content
{ "entity_id": "post-guardrails",
  "fields": { "title": "When Your AI Builds Guardrails",
              "platform": "Blog + X",
              "author": "jiji", "date": "2026-02-20" } }

# Calcifer does voice pass, submits for review
POST /tdata/Posts('post-guardrails')/Temper.SubmitForReview

# Rita approves → moves to Scheduled
POST /tdata/Posts('post-guardrails')/Temper.Approve

# Can't publish without review (guard):
POST /tdata/Posts('post-guardrails')/Temper.Publish
# ✓ works because Approve set has_review = true`,
    why: 'Without Temper, "is this post reviewed?" is a boolean someone forgot to set. With Temper, the review state is a verified transition — Publish requires Approve to have happened first. The calendar view reads directly from the entity fields. Webhook on Approve notifies Calcifer to schedule the actual post.'
  },

  'agent-vitals': {
    entity: 'AgentStatus',
    states: 'Online → Degraded → Offline (+ heartbeat counter)',
    spec: `# agent-status.ioa.toml
[entity]
name = "AgentStatus"
initial_status = "Online"

[vars]
heartbeat_count = { type = "counter", init = 0 }
missed_beats = { type = "counter", init = 0 }

[[actions]]
name = "Heartbeat"
from = ["Online", "Degraded"]
to = "@same"
effects = [{ increment = { var = "heartbeat_count" } }]

[[actions]]
name = "MissBeat"
from = ["Online"]
to = "Degraded"
effects = [{ increment = { var = "missed_beats" } }]

[[actions]]
name = "FailMultiple"
from = ["Degraded"]
to = "Offline"

[[actions]]
name = "Recover"
from = ["Degraded", "Offline"]
to = "Online"`,
    flow: `# Each agent heartbeat → fires Heartbeat action
POST /tdata/AgentStatuses('haku')/Temper.Heartbeat
X-Tenant-Id: vitals

# Monitor detects missed heartbeat
POST /tdata/AgentStatuses('jiji')/Temper.MissBeat

# Dashboard reads all agent states
GET /tdata/AgentStatuses
# Returns status, heartbeat_count, missed_beats per agent

# Sparkline data comes from trajectory table:
SELECT created_at, action FROM trajectories
WHERE entity_id = 'haku' AND tenant = 'vitals'
ORDER BY created_at DESC LIMIT 24`,
    why: 'Agent health isn\'t just "up or down" — it\'s a state machine. An agent that missed one heartbeat is Degraded, not Offline. The counter tracks heartbeats for the sparkline. The trajectory table gives you the activity history. Temper makes "is this agent healthy?" a verified question, not a guess from the last log timestamp.'
  }
};

function renderTemperSection(containerId, appKey) {
  const cfg = TEMPER_SECTIONS[appKey];
  if (!cfg) return;
  document.getElementById(containerId).innerHTML = `
    <details class="group">
      <summary class="flex items-center gap-2 cursor-pointer text-txt-3 hover:text-txt-2 transition-colors mb-4">
        <span class="text-[11px] font-mono font-medium uppercase tracking-[0.08em]">⚙ How this uses Temper</span>
        <svg class="w-3 h-3 transition-transform group-open:rotate-90" fill="currentColor" viewBox="0 0 20 20"><path d="M6 6l8 4-8 4z"/></svg>
      </summary>
      <div class="glass rounded-xl p-4 sm:p-6 space-y-5 text-sm">
        <div>
          <div class="text-[11px] font-mono font-medium uppercase tracking-[0.08em] text-txt-3 mb-2">Entity: ${cfg.entity}</div>
          <p class="text-txt-2 leading-relaxed mb-3">States: <code class="text-[13px] font-mono bg-s3 px-1.5 py-0.5 rounded">${cfg.states}</code></p>
          <pre class="glass rounded-lg p-3 text-[13px] font-mono text-txt-2 leading-relaxed overflow-x-auto whitespace-pre">${cfg.spec}</pre>
        </div>
        <div>
          <div class="text-[11px] font-mono font-medium uppercase tracking-[0.08em] text-txt-3 mb-2">API Flow</div>
          <pre class="glass rounded-lg p-3 text-[13px] font-mono text-txt-2 leading-relaxed overflow-x-auto whitespace-pre">${cfg.flow}</pre>
        </div>
        <div>
          <div class="text-[11px] font-mono font-medium uppercase tracking-[0.08em] text-txt-3 mb-2">Why Temper?</div>
          <p class="text-txt-2 leading-relaxed">${cfg.why}</p>
        </div>
      </div>
    </details>
  `;
}
