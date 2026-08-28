# Plan
1. Vendor the five workflows. (This PR.)
2. Panel-review + records + merge through the gates' own contract.
3. After merge: add the four gate contexts to branch protection (keeping the
   kernel's existing required checks).
4. Expected end state: any temper PR needs the design chain, a decision log,
   proof (or docs-only skip), and a validated panel record to merge.
