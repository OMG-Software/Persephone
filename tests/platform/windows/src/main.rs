#[cfg(windows)]
mod command_line;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
fn main() {
    windows::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("The AppContainer feasibility probe requires Windows.");
    std::process::exit(2);
}
