//! afterglowsvc.exe: Windows only
#[cfg(windows)]
fn main() {
    afterglow::platform::windows::svc::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("afterglowsvc is the Windows service; run afterglow directly here");
    std::process::exit(2);
}
