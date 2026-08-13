//! Every on-screen window of a pid, straight from the window server.
//!
//! Exists because AX is not a reliable witness here: KakaoTalk publishes zero
//! `AXWindows` while it is in the background, so an AX-based check cannot tell
//! "no window opened" from "the app stopped talking to us". `CGWindowList` is
//! answered by the window server itself and does not care who is frontmost.
//!
//! Usage: list_windows <pid>
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pid: i32 = args[1].parse().expect("usage: list_windows <pid>");
    let windows = cua_capture::list_windows().expect("list_windows");
    let mine: Vec<_> = windows.into_iter().filter(|w| w.pid == pid).collect();
    println!("{} window(s) for pid {pid}", mine.len());
    for w in mine {
        println!(
            "  id={} layer={} on_screen={} title={:?} frame={:?}",
            w.id, w.layer, w.on_screen, w.title, w.frame
        );
    }
}
