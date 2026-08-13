//! Does `apps::activate` actually bring an app forward from *this* process?
//!
//! macOS restricts cooperative activation: a process that is not itself
//! frontmost may not be allowed to hand the foreground to someone else. Both
//! the click fallback and `press_key` call `activate` and then poll for the
//! result, so whether it ever works decides whether those steps are
//! load-bearing or decoration.
//!
//! Usage: activate_probe <pid> [seconds]
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let pid: i32 = a[1].parse().expect("usage: activate_probe <pid> [seconds]");
    let secs: u64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);

    println!("frontmost before: {:?}", cua_core::frontmost_pid());
    let accepted = cua_core::activate(pid);
    println!("activate({pid}) returned {accepted}");

    let start = std::time::Instant::now();
    let mut landed = None;
    while start.elapsed() < std::time::Duration::from_secs(secs) {
        if cua_core::frontmost_pid() == Some(pid) {
            landed = Some(start.elapsed());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    match landed {
        Some(d) => println!("frontmost after: {pid} — took {} ms", d.as_millis()),
        None => println!(
            "frontmost after: {:?} — never became frontmost within {secs}s",
            cua_core::frontmost_pid()
        ),
    }
}
