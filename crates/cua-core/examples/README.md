# cua-core examples

These are integration probes for behavior that unit tests cannot exercise
without a live macOS desktop. Most require both Accessibility and Screen
Recording permissions on the launching process.

| Example | Purpose | Typical target | Permission | Status |
|---|---|---|---|---|
| `activate_probe` | Measure whether cooperative app activation succeeds | Any native app | Accessibility | Experimental diagnostic |
| `hover_check` | Compare tree and pixel evidence for pid-routed hover | Chrome, Safari, Finder | Both | Regression reproduction |
| `keyboard_probe` | Inspect focus and text read-back around pid-routed keys | TextEdit | Accessibility | Live regression diagnostic |
| `menu_life` | Measure pop-up lifetime and keyboard/menu reachability | KakaoTalk, TextEdit | Both | Regression reproduction |
| `mouse_verify` | Verify right-click, modifier-click, drag, hover, and wheel arms | TextEdit or a chosen app | Both | Integration probe |
| `popup_capture_probe` | Compare shell and in-process capture of pop-up window ids | Calculator, TextEdit | Both | Regression reproduction |
| `popup_visibility` | Characterize on-screen state for context and menu-bar pop-ups | TextEdit | Both | Regression reproduction |
| `scroll_check` | Compare wheel delivery with key and idle controls | TextEdit, browser fixture | Both | Regression reproduction |
| `window_click_probe` | Exercise elementless click delivery and its refusal gates | Harmless area in any app | Both | Integration probe |
| `window_timeout_probe` | Repeat `get_app_state` and time it, to check the `CannotComplete`-vs-`NoWindow` retry | KakaoTalk, Telegram | Accessibility | Regression reproduction |

The probes may send real process-routed input. Read each source file's usage and
choose reversible targets before running it.
