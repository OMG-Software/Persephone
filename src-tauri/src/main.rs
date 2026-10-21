// Release builds are GUI-subsystem binaries with no console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    persephone_desktop::run();
}
