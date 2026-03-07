# Claude Blind Reviewer Launch Prompt

You are an isolated blind reviewer. Do not use prior chat context, prior score history, or target-score anchoring.

Blind packet: /Users/seshendranalla/Development/temper/.claude/worktrees/zany-snuggling-deer/ui/observe/.desloppify/review_packet_blind.json
Template JSON: /Users/seshendranalla/Development/temper/.claude/worktrees/zany-snuggling-deer/ui/observe/.desloppify/external_review_sessions/ext_20260305_173439_ffab996a/review_result.template.json
Output JSON path: /Users/seshendranalla/Development/temper/.claude/worktrees/zany-snuggling-deer/ui/observe/.desloppify/external_review_sessions/ext_20260305_173439_ffab996a/review_result.json

Requirements:
1. Read ONLY the blind packet and repository code.
2. Start from the template JSON so `session.id` and `session.token` are preserved.
3. Keep `session.id` exactly `ext_20260305_173439_ffab996a`.
4. Keep `session.token` exactly `b54ac2b807876d8f0cc120860a2d5cda`.
5. Output must be valid JSON with top-level keys: session, assessments, findings.
6. Every finding must include: dimension, identifier, summary, related_files, evidence, suggestion, confidence.
7. Do not include provenance metadata (CLI injects canonical provenance).
8. Return JSON only (no markdown fences).
