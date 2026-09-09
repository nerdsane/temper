# ARN-237: CSDL attribute escaping merge

The user authorized completing and merging existing Temper PR #409 on 2026-09-09.

Preserve the PR's scoped outcome: emitted XML attribute values round-trip without injection or normalization loss, and parsed attribute entities decode correctly. Review the reported malformed-entity/defaulting behavior and resolve defects introduced by this change without expanding into the deferred truncated-schema and startup-recovery work.

The broader ARN-237 remains open until its separately sequenced strict-parser requirements are complete. The implementation belongs to Temper's generic specification kernel.

Success: scoped verification and review pass on the final PR head; the existing PR merges through the required workflow, followed by applicable kernel deployment verification.
