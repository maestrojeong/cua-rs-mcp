# cua-hid examples

These programs investigate low-level event routing. They can synthesize input
and several intentionally exercise private SkyLight routes. Run only in a GUI
test session against harmless controls.

| Example | Purpose | Typical target | Permission | Status |
|---|---|---|---|---|
| `click_probe` | Compare public pid posting with older control arms | Any known-good checkbox | Accessibility for read-back | Historical reproduction |
| `event_spy` | Dump a listen-only session event tap | Whole test session | Accessibility | Experimental diagnostic |
| `focus_probe` | Observe synthetic focus protocol and click acceptance | KakaoTalk/custom controls | Accessibility + Screen Recording | Regression diagnostic |
| `menu_probe` | Compare quiet and activated delivery for menu controls | KakaoTalk | Accessibility + Screen Recording | Regression reproduction |
| `pid_click_probe` | Find the minimum sufficient pid-routed click recipe | Any known-good control | Accessibility + Screen Recording | Historical experiment |
| `slps_click_probe` | Characterize SkyLight focus and post primitives | Any known-good control | Accessibility + Screen Recording | Experimental SPI probe |

These are not stable public interfaces. Their command-line flags exist to keep
past experiments reproducible when a macOS release changes event behavior.
