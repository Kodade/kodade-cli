# Releasing Ködade CLI

1. Bump the version in the workspace's three `Cargo.toml` package manifests.
2. Add the release notes to the appropriate `CHANGELOG` section in `README.md`, if applicable.
3. Commit the changes, then tag and push the release:

   ```sh
   git tag v<version>
   git push --tags
   ```

Pushing a `v*` tag starts the release workflow. It builds `kodade-cli` for
macOS arm64/x86_64 and Linux arm64/x86_64, packages each binary with
`LICENSE`, `NOTICE`, and `README.md`, and publishes four tarballs plus
`SHA256SUMS` to a GitHub Release.

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
