//! Windows-only resource build (issue #35): embed the repo icon
//! (`assets/activity-daemon.ico`) as the exe icon so Explorer, the taskbar
//! and shortcuts show the brand mark instead of the Rust default. Other
//! targets no-op - the ubuntu CI leg has no Windows resource compiler, and
//! the linux server binary needs no icon.

fn main() {
    if std::env::var("CARGO_CFG_WINDOWS").is_err() {
        return;
    }
    println!("cargo:rerun-if-changed=../../assets/activity-daemon.ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../assets/activity-daemon.ico");
    if let Err(e) = res.compile() {
        panic!("cannot embed the exe icon: {e}");
    }
}
