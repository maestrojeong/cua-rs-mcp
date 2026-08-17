//! Ad-hoc manual check for the CannotComplete-vs-NoWindow fix.
//!
//! Not wired into the test suite: it needs a live GUI session with the named
//! app actually running and a real window on screen, which a CI runner does
//! not have. Kept as a runnable record of how the fix was verified rather than
//! deleted once the bug closed.
//!
//! ```console
//! $ cargo run -p cua-core --example window_timeout_probe -- KakaoTalk
//! $ cargo run -p cua-core --example window_timeout_probe -- Telegram
//! ```
use cua_core::{Cua, StateOptions};

fn main() {
    let app = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "KakaoTalk".into());
    let cua = Cua::new();
    for i in 1..=5 {
        let start = std::time::Instant::now();
        match cua.get_app_state(
            &app,
            StateOptions {
                include_screenshot: false,
                ..Default::default()
            },
        ) {
            Ok(st) => println!(
                "[{i}] ok in {:?}: window={:?} elements={}",
                start.elapsed(),
                st.window_title,
                st.node_count
            ),
            Err(e) => println!("[{i}] err in {:?}: {e}", start.elapsed()),
        }
    }
}
