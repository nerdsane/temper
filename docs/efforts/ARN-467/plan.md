# Implementation plan

1. Reproduce undeclared field mutation and ignored pre-state comparisons using real actors under deterministic simulation.
2. Add opt-in IOA contracts and preserve them in compiled tables.
3. Reproduce generic create, PATCH, PUT and DELETE bypasses through HTTP and direct state APIs, then reject them before mutation.
4. Align reaction simulator field projection with production and exercise actual multi-entity factory contracts.
5. Run parser, transition, actor and HTTP regressions. review and merge the kernel dependency, pin it in TemperPaw, then prove the deployed factory.
