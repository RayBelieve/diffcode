# Local changes

This repository is a modified copy of [DiffCode](https://github.com/ihou/DiffCode) (GPL-3.0).
The upstream project and its documentation are kept as they were, except for the changes below.

## Windows fixes and additions

- **Terminal shell selection.** The integrated terminal no longer prefers the
  Microsoft Store `WindowsApps` execution alias. A portable PowerShell 7 install
  (for example `D:\PowerShell-7.7.0-preview.3-win-x64\pwsh.exe`) is preferred when
  present, and Settings → **Terminal shell** accepts an explicit executable path.
  Existing terminals keep their shell; new terminals use the setting.
- **Minimize crash.** With the terminal open, minimizing the window made the
  remaining client height negative, so `Rect::split_top` called
  `clamp(0.0, -1.0)` and the release build aborted (`0xc0000409`). The layout now
  clamps the post-separator height to zero, and `Rect` splits tolerate negative
  sizes.
- **Follow code changes.** An optional mode (Settings → **Follow code changes**,
  or `Ctrl+Alt+F`) selects files changed by another program, opens their diff and
  scrolls to the new hunk. It ignores the viewer's own saves and does not move the
  view while the editor has unsaved changes.
- **WSL agent.** Repositories inside WSL no longer poll `git status` from Windows.
  A small static Linux helper (`windows/wsl-agent`) runs in the distro: it watches
  the work tree with inotify and reports changes, and executes git requests over
  the same stdin/stdout, so status, diffs and commits do not start `wsl.exe` each
  time. If the helper cannot start, DiffCode falls back to polling.

## Build (Windows)

```powershell
cd windows
cargo test --workspace --locked
cargo build -p diffcode --release

# The WSL helper is a separate Linux binary, cross-compiled on Windows.
rustup target add x86_64-unknown-linux-musl
$env:RUSTFLAGS = "-C linker=rust-lld -C link-self-contained=yes"
$env:CARGO_TARGET_DIR = "target/wsl-agent"
cargo build --manifest-path wsl-agent/Cargo.toml --target x86_64-unknown-linux-musl --release --locked
New-Item -ItemType Directory -Force target/release/wsl | Out-Null
Copy-Item target/wsl-agent/x86_64-unknown-linux-musl/release/diffcode-wsl-agent target/release/wsl/
```

The executable is `windows\target\release\DiffCode.exe`. It is not code-signed.

## Upstream

- Source: https://github.com/ihou/DiffCode
- License: GNU GPL v3.0, see [LICENSE](LICENSE).
