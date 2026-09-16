# Contract

The review gate may accept either its existing current-head review record or a separate explicit owner waiver. A waiver binds the full current SHA, repository, PR, trusted GitHub comment identity, historical reviewed ancestor, and digest of the unchanged historical record. It requires reason, authorization, and evidence links. It never edits the historical commit or claims a new reviewer run. Other required checks are unaffected.

Only GitHub User comments associated as OWNER, MEMBER or COLLABORATOR qualify, matching existing owner-ruling authority. The exact first line is REVIEW-WAIVER: @<40-character lowercase SHA>, followed by one JSON object. Quoted instructions, bot comments and stale-head commands do not qualify. A malformed latest matching authorized command fails closed. GitHub comment identity is fetched by trusted base-branch code, never supplied by PR code. An agent may post on behalf of the owner only with explicit user authorization and must record that direction accurately; a personal GitHub token cannot distinguish delegated use from the human typing.

State model: no qualifying command -> existing review path; valid command with matching historical evidence -> owner-waiver result; malformed qualifying command or unavailable GitHub/ancestry -> failure. New head -> old waiver inapplicable. Model-review success is never inferred from a waiver.
