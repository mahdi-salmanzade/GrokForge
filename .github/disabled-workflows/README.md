# Archived GitHub Actions workflows

CI, nightly checks, and release publishing are disabled. Their definitions live here,
outside `.github/workflows/`, so pushes, pull requests, schedules, tags, and manual
dispatches cannot run them. GitHub Actions is also disabled in the repository settings.

Run the checks locally before pushing:

```sh
cargo build --locked --workspace
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
INSTA_UPDATE=no cargo test --locked --workspace
cargo deny check
```

To restore automation, review the archived definitions, move the selected YAML files
back into `.github/workflows/`, and enable GitHub Actions in the repository settings.
Moving files back without enabling Actions does not restore automation. The release
workflow publishes assets for `v*` tags; the nightly workflow includes a daily schedule.
