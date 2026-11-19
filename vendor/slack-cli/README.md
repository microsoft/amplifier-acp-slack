# Windows noninteractive Slack CLI build

This directory identifies `constellation-headless-1`, a Windows-only derivative
of Slack CLI 4.12.0 at the exact upstream source in `source-lock.json`.
It is not Slack's published executable and must not use its archive checksum.

The patch returns Lip Gloss's existing dark-background fallback when Windows
setup explicitly supplies nonempty `NO_COLOR`, before the package initializer
opens console devices. Other platforms and ordinary interactive colour detection
are unchanged. This does not change authentication, hooks, requests, or deadlines.
The connector retains its hidden child launch, redirected streams, and Job Object.

CI runs `scripts/build-slack-cli.py --output <fresh-directory-outside-checkout>`
with already-installed Go 1.27.3. An optional `--source-archive` reuses the exact
locked upstream archive. The script verifies archive/file/patch hashes, applies
the patch without fuzz, and builds Windows amd64 with CGO disabled, matching the
upstream Windows x64 compatibility choice on Windows ARM.

Upstream `.goreleaser.yml`, `.circleci/config.yml`, and `Makefile` supply the
`-s -w` version linker flags. This build additionally records `-mod=readonly`,
`-trimpath`, and `-buildvcs=false` for fixed module inputs and reproducibility.
It does not run upstream `go mod tidy`. Module downloads use Go's public checksum
database; toolchain auto-download and ambient Go workspace/flag overrides are off.

The output is exactly `slack.exe`, the unchanged upstream `LICENSE`, and
`provenance.json`. A separate sibling `.work` directory retains source and build
logs, including failures. Existing output/work directories are never reused.
No executable is run by the builder. CI runtime and no-visible-console acceptance
must still use the actual packaged CLI and its real hook child path.
End-user setup does not fetch source or build this tool.
