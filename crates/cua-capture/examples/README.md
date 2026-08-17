# cua-capture examples

| Example | Purpose | Typical target | Permission | Status |
|---|---|---|---|---|
| `list_windows` | Enumerate window-server windows for one pid when AX cannot witness them | KakaoTalk or any background app | Screen Recording | Diagnostic utility |

Run in a GUI session. Window titles and frames can expose private on-screen
information, so use the probe only against an intended target process.
