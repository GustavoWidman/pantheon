# Validated source releases

`Cargo.toml` owns the release version; `Cargo.lock` must agree. A merge to
`main` automatically runs the release workflow. Validation must pass before
publication. Publishing source does not switch a service or authorize a state
conversion, deployment or cutover.

## Commit and squash metadata

Use Conventional Commits for every non-merge commit, the PR title, and the final
squash commit subject, for example:

```text
docs(release): document validated squash publication
```

PR CI checks the branch commits and title. A squash merge creates a different
commit, so explicitly set a conventional squash subject and verify the resulting
subject along with the expected parent and reviewed source tree. Do not assume
that a validated branch tree proves the metadata of the eventual squash commit.
The automatic release validates the actual push range on `main` again.

## Publication and forward recovery

A release PR normally increments the stable SemVer version. The existing
`no-release` label exempts a PR from the version-bump gate when it is not a new
version, such as a documentation-only forward repair for a version whose
publication was blocked. This exemption does not skip Rust, Nix, conventional
commit or release validation.

If validation fails after merge, preserve the failed run and inspect its exact
stage. Do not rewrite shared history, move an existing version tag, weaken a gate
or repeatedly rerun an unchanged invalid commit. Repair forward through a new
reviewed PR. If retaining an unpublished version, verify its tag/release state;
never assume a failed or interrupted operation had no effect.

The release workflow binds publication to its validated checkout, never
replaces an existing tag, and does not activate the package. Verify the terminal
workflow result, version tag's commit and tree, release metadata, and package
source separately. A failed metadata check with publication skipped is not
release success, even when compilation and tests passed.
