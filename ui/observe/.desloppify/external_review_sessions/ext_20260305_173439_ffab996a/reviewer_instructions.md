# External Blind Review Session

Session id: ext_20260305_173439_ffab996a
Session token: b54ac2b807876d8f0cc120860a2d5cda
Blind packet: /Users/seshendranalla/Development/temper/.claude/worktrees/zany-snuggling-deer/ui/observe/.desloppify/review_packet_blind.json
Template output: /Users/seshendranalla/Development/temper/.claude/worktrees/zany-snuggling-deer/ui/observe/.desloppify/external_review_sessions/ext_20260305_173439_ffab996a/review_result.template.json
Claude launch prompt: /Users/seshendranalla/Development/temper/.claude/worktrees/zany-snuggling-deer/ui/observe/.desloppify/external_review_sessions/ext_20260305_173439_ffab996a/claude_launch_prompt.md
Expected reviewer output: /Users/seshendranalla/Development/temper/.claude/worktrees/zany-snuggling-deer/ui/observe/.desloppify/external_review_sessions/ext_20260305_173439_ffab996a/review_result.json

Happy path:
1. Open the Claude launch prompt file and paste it into a context-isolated subagent task.
2. Reviewer writes JSON output to the expected reviewer output path.
3. Submit with the printed --external-submit command.

Reviewer output requirements:
1. Return JSON with top-level keys: session, assessments, findings.
2. session.id must be `ext_20260305_173439_ffab996a`.
3. session.token must be `b54ac2b807876d8f0cc120860a2d5cda`.
4. Include findings with required schema fields (dimension/identifier/summary/related_files/evidence/suggestion/confidence).
5. Use the blind packet only (no score targets or prior context).
