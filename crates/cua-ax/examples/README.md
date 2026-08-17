# cua-ax examples

These programs inspect or exercise the macOS Accessibility API directly. They
are diagnostics and regression reproductions, not supported end-user commands.
Unless noted otherwise, run them in a GUI session from a process with the
Accessibility permission.

| Example | Purpose | Typical target | Permission | Status |
|---|---|---|---|---|
| `aim_probe` | Compare frame centers and activation points with AX hit testing | Any app; off-Space windows are useful | Accessibility | Experimental diagnostic |
| `ax_poke` | Diagnose lazy Chromium/Electron accessibility enablement | Slack, Chrome, Electron apps | Accessibility | Regression diagnostic |
| `enhanced_probe` | Test whether enhanced accessibility exposes more actions | KakaoTalk/custom controls | Accessibility | Experimental probe |
| `focus_window` | Focus and raise one AX window by title | Multi-window native apps | Accessibility | Utility example |
| `kakao_probe` | Test selection attributes as an activation mechanism | KakaoTalk | Accessibility | Historical experiment |
| `menu_item_press` | Reproduce first-press menu activation and delayed read-back | Calculator, TextEdit | Accessibility | Regression reproduction |
| `point_probe` | Print the AX element at a screen point | Any app | Accessibility | Utility example |
| `read_at` | Read a value by label without unreliable background hit testing | Any background app | Accessibility | Utility example |
| `windows_probe` | Enumerate all AX windows for a process | KakaoTalk/multi-window apps | Accessibility | Diagnostic |

Every action-oriented probe should be aimed at harmless, reversible controls.
