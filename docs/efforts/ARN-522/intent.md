# Intent

A caller that has just been told an action is applied must be able to read
the result. Today a single-entity OData read is served from the entity
catalog, and the catalog is written by a background queue, so a read that
follows a dispatch — even one made inside the same request, after the kernel
answered — can return the row as it was before. Genesis's merge is the case:
`Repository.MergePullRequest` applies its sub-writes, answers, and the REST
layer's read-back of the PullRequest still says `Approved` with no merge
commit, so the response reports the pre-merge tip. The merge itself is
correct; the read is behind. Tracked as
[ARN-522](https://linear.app/arni-build/issue/ARN-522), under
[ARN-467](https://linear.app/arni-build/issue/ARN-467); it blocks
`arni-labs/genesis` PR #50's CI at the merge step.
