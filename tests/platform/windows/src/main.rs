#[cfg(windows)]
mod command_line;
#[cfg(windows)]
mod screen;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
fn main() {
    // The screen capture probe (ADR-0053) has its own entry; the containment
    // gate runs for every other invocation.
    if let Some(code) = screen::dispatch() {
        std::process::exit(code);
    }
    windows::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("The AppContainer feasibility probe requires Windows.");
    std::process::exit(2);
}
