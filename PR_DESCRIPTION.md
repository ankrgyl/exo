Sandbox creation currently supports starting from a base image or restoring a fully materialized snapshot, but it cannot warm a sandbox from source before exposing it. Add `CreateSandboxFromRecipeRequest` with an optional starting snapshot and an ordered list of initialization steps. The snapshot, when present, is always restored first. Steps can clone a GitHub repository and run commands inside the acquired sandbox.

A GitHub step clones one branch instead of fetching the entire repository. Omitting the branch uses the repository's default branch, and an optional SHA is checked out detached after the clone. This keeps snapshots as materialized filesystem state while recipes describe how to build that state.

Private repositories can reference an ExoHarness key secret. The token is resolved from the sandbox's scope and passed only to `git clone` through a command-scoped Git extra header. It is not placed in argv, written to the remote URL, or retained in the sandbox after the recipe finishes. Failed recipes terminate the partially initialized sandbox before it is persisted.

Validation:

- `cargo check -p executor`
- `cargo clippy -p exoharness --features basic-backend --lib -- -D warnings`
- `cargo test -p exoharness --features basic-backend --lib basic_tests::restore_sandbox_creates_a_new_target_without_a_cold_acquire -- --exact`
- `cargo test -p exoharness --features basic-backend --lib basic_tests::github_recipe_checks_out_a_public_repository -- --ignored --exact`
- Ignored live coverage is included for a private repository using `GITHUB_TEST_REPOSITORY`, `GITHUB_TEST_SHA`, and `GITHUB_TEST_TOKEN` or `GITHUB_TOKEN`.
