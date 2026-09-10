# Releasing Ködade CLI

1. Bump the version in the workspace's three `Cargo.toml` package manifests and
   refresh the three workspace-package entries in `Cargo.lock` without changing
   dependency versions.
2. Add the release notes to `CHANGELOG.md`. For v0.3.0, state that the release
   supports Linux and macOS only and include the v0.2.1 migration caveat: an
   existing v0.2.1 daemon cannot live-handoff its panes.
3. Run the full Unix release gates and record their final evidence before
   creating the tag. A release candidate is not a published release.
4. Commit the changes, then tag and push the release:

   ```sh
   git tag v<version>
   git push --tags
   ```

Pushing a `v*` tag starts the release workflow. Publication requires the shared
Linux/macOS CI workflow to pass, including formatting, Clippy, workspace tests,
and the real-PTY rendering and keyboard smoke tests. Build success alone cannot
publish a release. The workflow builds
`kodade-cli` for macOS arm64/x86_64 and Linux arm64/x86_64, packages each binary
with `LICENSE`, `NOTICE`, and `README.md`, and publishes exactly four tarballs
plus `SHA256SUMS` to a GitHub Release. Do not add Windows archives or describe
native Windows as supported for this release.

A final `homebrew` job renders `Formula/kodade-cli.rb` from `SHA256SUMS` with
`scripts/homebrew-formula.sh` and pushes it to
[Kodade/homebrew-tap](https://github.com/Kodade/homebrew-tap). It needs the
repository secret `HOMEBREW_TAP_TOKEN`: a fine-grained personal access token
with `Contents: read and write` on `Kodade/homebrew-tap` only. Without the
secret the job prints the rendered formula and warns instead of failing;
commit it to the tap by hand in that case.

To test the installer locally against a published release, run the script with
the release repository's normal latest-release endpoint:

```sh
curl -fsSL https://raw.githubusercontent.com/Kodade/kodade-cli/main/install.sh | sh
```

For a non-default destination, set `KODADE_INSTALL_DIR` before running it. The
installer downloads the latest matching archive and verifies it against
`SHA256SUMS` before installing.

The CLI updater consumes the GitHub release API and requires every published
release to retain its matching platform archives and `SHA256SUMS`. Stable
checks use the latest non-prerelease release; preview checks select the newest
published prerelease.
