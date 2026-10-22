#[cfg(windows)]
mod windows;

#[cfg(windows)]
fn main() {
    windows::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("The credential vault probe requires Windows.");
    std::process::exit(2);
}
