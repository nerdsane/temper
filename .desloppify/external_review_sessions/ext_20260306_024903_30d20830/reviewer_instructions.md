# External Blind Review Session

Session id: ext_20260306_024903_30d20830
Session token: c157e2d6c882cfb9d0cbaa870026322d
Blind packet: /Users/seshendranalla/Development/temper/.claude/worktrees/zany-snuggling-deer/.desloppify/review_packet_blind.json
Template output: /Users/seshendranalla/Development/temper/.claude/worktrees/zany-snuggling-deer/.desloppify/external_review_sessions/ext_20260306_024903_30d20830/review_result.template.json
Claude launch prompt: /Users/seshendranalla/Development/temper/.claude/worktrees/zany-snuggling-deer/.desloppify/external_review_sessions/ext_20260306_024903_30d20830/claude_launch_prompt.md
Expected reviewer output: /Users/seshendranalla/Development/temper/.claude/worktrees/zany-snuggling-deer/.desloppify/external_review_sessions/ext_20260306_024903_30d20830/review_result.json

Happy path:
1. Open the Claude launch prompt file and paste it into a context-isolated subagent task.
2. Reviewer writes JSON output to the expected reviewer output path.
3. Submit with the printed --external-submit command.

Reviewer output requirements:
1. Return JSON with top-level keys: session, assessments, findings.
2. session.id must be `ext_20260306_024903_30d20830`.
3. session.token must be `c157e2d6c882cfb9d0cbaa870026322d`.
4. Include findings with required schema fields (dimension/identifier/summary/related_files/evidence/suggestion/confidence).
5. Use the blind packet only (no score targets or prior context).
