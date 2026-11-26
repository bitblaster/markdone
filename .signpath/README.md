# SignPath setup

Markdone's Windows builds are signed through the [SignPath Foundation](https://signpath.org)
open-source program (see [docs/code-signing.md](../docs/code-signing.md)). This folder holds
the artifact configurations the SignPath project uses; the release workflow
(`.github/workflows/release.yml`) submits the signing requests.

| File | Signs | Submitted by the step |
|---|---|---|
| `artifact-configurations/markdone-exe.xml` | `markdone.exe` | *Sign the executable (SignPath)* |
| `artifact-configurations/markdone-installer.xml` | `markdone-v<version>-x86_64-pc-windows-msvc-setup.exe` | *Sign the installer (SignPath)* |

The executable is signed first; the portable zip and the installer are then built around
the signed executable, and the installer is signed in turn. The `.sha256` files are
computed last, from the signed files (the self-updater checks them).

## In the SignPath portal

Once the SignPath Foundation application is approved:

1. **Trusted build system**: in the organization, add the predefined *GitHub.com* trusted
   build system, and link it to the project (step 2).
2. **Project**: create a project for the repository (e.g. slug `markdone`), with the
   repository URL `https://github.com/bitblaster/markdone`.
3. **Artifact configurations**: in the project, create two artifact configurations and
   paste the XML of each file above into it (e.g. slugs `markdone-exe` and
   `markdone-installer`). When a file here changes, update the portal's copy too: the
   portal's version is the one used.
4. **Signing policy**: create the release signing policy (e.g. slug `release-signing`)
   with the SignPath Foundation certificate, **manual approval** required for every
   request, and yourself as submitter (the API token's user) and approver.
5. **API token**: create a CI user (or use your own) with *submitter* permission on the
   signing policy, and generate its API token.
6. Optionally install the SignPath GitHub App on the repository (SignPath requires it
   only to evaluate audit logs and pipeline policies).

## In the GitHub repository (Settings → Secrets and variables → Actions)

| Kind | Name | Value |
|---|---|---|
| Secret | `SIGNPATH_API_TOKEN` | the API token of step 5 |
| Variable | `SIGNPATH_ORGANIZATION_ID` | the organization's ID (portal: organization settings) |
| Variable | `SIGNPATH_PROJECT_SLUG` | e.g. `markdone` |
| Variable | `SIGNPATH_SIGNING_POLICY_SLUG` | e.g. `release-signing` |
| Variable | `SIGNPATH_EXE_CONFIGURATION_SLUG` | e.g. `markdone-exe` |
| Variable | `SIGNPATH_INSTALLER_CONFIGURATION_SLUG` | e.g. `markdone-installer` |
| Variable | `SIGNPATH_ENABLED` | `true` to sign; anything else (or unset) publishes unsigned builds |

With `SIGNPATH_ENABLED` set to `true`, a missing secret or variable fails the release
instead of publishing unsigned files.

## Notes

- The signing steps run only for a `v*` tag pushed to `bitblaster/markdone`, on GitHub's
  hosted `windows-latest` runner: never for a manual run, a fork or a pull request (the
  workflow has no pull-request trigger).
- Each request waits up to an hour for the manual approval
  (`wait-for-completion-timeout-in-seconds: 3600`); after that the job fails and the
  release must be re-run.
- The configurations enforce the product name `Markdone` and the product version
  passed by the workflow (Cargo.toml's `version`, e.g. `0.6.0`). The executable gets
  them from `build.rs` (winresource), the installer from the `VersionInfo*` settings of
  `packaging/windows/markdone.iss`. **To check on the first signed release:** which
  version field SignPath compares (the text version `0.6.0` or the numeric `0.6.0.0`)
  is not stated in its documentation; if a request is rejected on `product-version`,
  adjust the attribute (or the version resource) accordingly.
- The Inno Setup uninstaller (`unins000.exe`) is generated on the user's machine by the
  installer, so it is not signed by this process.
