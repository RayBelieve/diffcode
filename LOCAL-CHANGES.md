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

## Build (Windows)

```powershell
cd windows
cargo test --workspace --locked
cargo build -p diffcode --release
```

The executable is `windows\target\release\DiffCode.exe`. It is not code-signed.

## Upstream

- Source: https://github.com/ihou/DiffCode
- License: GNU GPL v3.0, see [LICENSE](LICENSE).
