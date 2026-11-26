# Code signing policy

Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by
[SignPath Foundation](https://signpath.org).

> **Status:** the signing process below is prepared but not active yet. Until it is,
> the Windows builds are published unsigned, and SmartScreen may warn about them.

## What is signed

Only the Windows files Markdone's own release workflow builds from this repository's
source code, for a tagged release:

- `markdone.exe`, the program, in both the installer and the portable zip
  (`markdone-v<version>-x86_64-pc-windows-msvc.zip`);
- the installer, `markdone-v<version>-x86_64-pc-windows-msvc-setup.exe`.

They are built on GitHub's hosted runners by
[`.github/workflows/release.yml`](../.github/workflows/release.yml) from the tagged commit,
and submitted to SignPath from that workflow; the artifact configurations are in
[`.signpath/`](../.signpath/). Nothing built elsewhere, and no third-party binary, is
signed. The Linux and macOS builds are not covered.

The `.sha256` checksum published next to each file is computed from the signed file.

## Roles

| Role | Members |
|---|---|
| Committers and reviewers | [Roberto Mozzicato (bitblaster)](https://github.com/bitblaster) |
| Approvers | [Roberto Mozzicato (bitblaster)](https://github.com/bitblaster) |

- **Committers** may change the source code without further review. Changes proposed by
  anyone else (pull requests) are reviewed by a reviewer before they are merged.
- **Approvers**: every signing request is approved by hand in SignPath before anything
  is signed; nothing is signed automatically.
- Multi-factor authentication is required, and enabled, for every member, on both GitHub
  and SignPath.

## Privacy

Markdone collects no data: no telemetry, no analytics, no accounts, no crash reports. It
connects to other systems only for the following, each with the purpose stated:

| When | Where | What is sent |
|---|---|---|
| At every start, unless turned off (*Preferences → Privacy*, "Check for a new version of Markdone at startup"; on by default), and on *Help → Check for Updates…* | `api.github.com` | A request for the latest Markdone release. |
| When you accept to install an update | `github.com` and GitHub's download servers | Requests for the release package and its checksum. |
| When you check a spelling language that the system has no dictionary for | `raw.githubusercontent.com` (LibreOffice's dictionaries repository) | Requests for that language's dictionary files. |
| Only if you turn on GitHub references (*Preferences → Privacy*; off by default) | `api.github.com` | The repository named in the document folder's `.git/config`, the issue/pull request numbers, user names and search text found or typed in the document, and the GitHub token you configured, if any. |
| When an open document shows an image by a web address (`![…](https://…)`) | The server in that address | A request for that image, as a web browser would. |
| When you export a PDF | The servers of the document's web images | The export runs in a Chromium-based browser installed on your computer (Chrome, Edge, Chromium or Brave), which fetches those images. |
| When you click a link | The link's address | It opens in your web browser. |

Like any web request, these reveal your IP address to the server. Markdone names itself
in its requests' user agent: `markdone/<version>` for the update check and download,
`markdone` for GitHub references and web images. Nothing else about you, your
computer or your documents is sent. The documents, the configuration
(`config.toml`), the session (open files, recent files) and the personal dictionary stay
on your computer.
