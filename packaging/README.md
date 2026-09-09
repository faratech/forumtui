# wftui release packaging

Two independent outputs, built and verified separately:

- **Windows** — a signed MSIX per architecture (x64, x86, arm64) plus a
  combined `.msixbundle`, under `packaging/msix/`.
- **Linux** — a plain release tarball for the host's native target, under
  `packaging/linux/`.

Proprietary, in-development software — no LICENSE file yet, so neither
artifact is meant for public redistribution.

## Linux

```bash
cd /web/wftui_app
packaging/linux/build-tarball.sh              # default (images) build
packaging/linux/build-tarball.sh --no-images  # the no-default-features build
```

Runs entirely on this box: `cargo build --release -p wftui` for the host's
native target, then stages the binary + a short README into
`dist/wftui-<version>-linux-<arch>.tar.gz` next to a `.sha256`. No
cross-compilation — this always builds for whatever target the machine
running the script is (same as `bin/wftui`, which is built the same way and
committed separately).

## Windows (MSIX, signed)

This half **cannot run on this Linux server** — `makeappx.exe` and
`signtool.exe` are Windows SDK tools with no Linux equivalent, and the
signing credentials in `/root/.sign` are meant to be used from a Windows
build box. What's checked in here is everything short of that:

- `packaging/msix/AppxManifest.template.xml` — the manifest, parameterized
  by `{{VERSION}}` / `{{ARCH}}`. Registers `wftui.exe` as a Desktop Bridge
  (`Windows.FullTrustApplication`) app with an **App Execution Alias**, so
  after install `wftui` is runnable from any shell with no PATH edit —
  the standard shape for a CLI tool shipped as MSIX (same idea as `winget`,
  `gh`).
- `packaging/msix/assets/` — Square44x44Logo, Square150x150Logo,
  Wide310x150Logo and StoreLogo (plus scale-200 variants), generated from
  `wftui/assets/wf-logo.png`. Regenerate if the logo changes:
  ```bash
  convert wftui/assets/wf-logo.png -filter Lanczos -resize 44x44 packaging/msix/assets/Square44x44Logo.png
  # ...same pattern for the other sizes; see git log on this file for the exact set.
  ```
- `packaging/msix/build-msix.ps1` — cross-builds all three architectures,
  packs each with `makeappx`, and signs every package (and the bundle)
  through the existing Azure Trusted Signing kit.

### One-time setup on the Windows build box

1. Rust (rustup) + the MSVC toolchain, and the Windows SDK (specifically the
   "MSIX Packaging Tool" / "Signing Tools for Desktop Apps" component, for
   `makeappx.exe`).
2. Run `packaging/msix/signing/install-dlib.ps1` to restore the signing library.
3. Set `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, and `AZURE_CLIENT_SECRET` in the environment. GitHub Actions reads these from repository secrets. Never commit their values.

EXEs are dual-signed before packaging: Fara Technologies LLC first, Mike Fara second. MSIX packages keep the single Mike Fara signature matching their existing publisher identity.

### Build + sign

```powershell
cd C:\path\to\wftui_app\packaging\msix
.\build-msix.ps1
```

Produces, in `C:\code\sign\dist\` by default:

- `wftui-<version>-x64.msix`
- `wftui-<version>-x86.msix`
- `wftui-<version>-arm64.msix`
- `wftui-<version>.msixbundle` (all three architectures, one installer —
  what you'd hand to `Add-AppxPackage` or winget)
- `wftui-<version>.SHA256SUMS.txt`

Each `.msix` is signed and verified (`signtool sign` + `signtool verify /pa`)
before the script moves on; a failure anywhere aborts the run non-zero.

Useful flags: `-Architectures x64` (skip the others for a quick local test),
`-SkipSign` (unsigned, sideload-only build), `-SkipBundle`, `-Version 0.2.0`
(override the version read from `wftui/Cargo.toml`), `-SignKitRoot` /
`-OutDir` (relocate either directory).

### Install / verify locally

```powershell
Add-AppxPackage -Path .\wftui-<version>.msixbundle
wftui.exe   # from any new shell, via the execution alias
```

A signed package installs without a Developer Mode toggle or an extra
trusted-root import, because Azure Trusted Signing issues a publicly
trusted certificate (`CN=Mike Fara, O=Mike Fara, L=White Plains, S=ny,
C=US` — must match `Identity/@Publisher` in the manifest template exactly,
or `signtool` rejects the package after packing).

### Why MSIX instead of a raw exe here

Known gaps in the main `CLAUDE.md` used to say "Windows packaging is
documentation-only" — this is what replaces that. MSIX was chosen over a
bare signed `.exe` because it gets `wftui` onto the user's PATH via the
execution alias with no installer UI to write, and because Azure Trusted
Signing already covers `.msix` (`sign.ps1` doesn't care about the
extension — same signing call as an `.exe`).
