//! Detection probe for issue #6: report which configured conferencing
//! processes currently hold active audio sessions, and whether the watcher
//! would auto-trigger right now.
//!
//! Usage (Windows):
//!   cargo run -p watcher-audio --example detect_probe -- Teams.exe Zoom.exe
//! Without arguments the daemon default list from core config is used.

#[cfg(windows)]
fn main() {
    if !wasapi::initialize_mta().is_ok() {
        eprintln!("warning: COM (MTA) initialization failed");
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let process_list = if args.is_empty() {
        activity_core::Config::default().audio.process_list
    } else {
        args
    };
    println!("process_list: {process_list:?}");

    println!("active audio sessions on render devices:");
    match watcher_audio::win_detector::active_session_processes() {
        Ok(sessions) if sessions.is_empty() => println!("  (none)"),
        Ok(sessions) => {
            for (pid, name) in sessions {
                println!("  pid={pid} process={name}");
            }
        }
        Err(e) => println!("  enumeration failed: {e:#} (daemon would fall back to process check)"),
    }

    let mut detector = watcher_audio::win_detector::ConferencingDetector::new(
        &process_list,
        watcher_audio::logging::Log::stderr_only(),
    );
    match watcher_audio::ConferenceWatcher::poll(&mut detector) {
        Some(hit) => println!(
            "would auto-trigger now: process={} detect_mode={}",
            hit.process,
            hit.mode.label()
        ),
        None => println!("would NOT auto-trigger now (no configured process in a call)"),
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("detect_probe is windows-only (WASAPI session enumeration)");
}
