use std::{env, fs, path::PathBuf, process};

use persephone_contracts::generated_typescript;

fn main() {
    let mut args = env::args_os().skip(1);
    let mode = args.next().unwrap_or_else(|| "--check".into());
    if args.next().is_some() {
        eprintln!("usage: export-contracts [--check|--write]");
        process::exit(2);
    }

    let output = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("generated/contracts.ts");
    let expected = generated_typescript();
    match mode.to_str() {
        Some("--check") => match fs::read_to_string(&output) {
            Ok(actual) if actual == expected => println!("TypeScript bindings are current."),
            Ok(_) => {
                eprintln!(
                    "{} is stale; run the explicit --write command.",
                    output.display()
                );
                process::exit(1);
            }
            Err(error) => {
                eprintln!("cannot read {}: {error}", output.display());
                process::exit(1);
            }
        },
        Some("--write") => {
            fs::write(&output, expected).unwrap_or_else(|error| {
                eprintln!("cannot write {}: {error}", output.display());
                process::exit(1);
            });
            println!("Updated {}", output.display());
        }
        _ => {
            eprintln!("usage: export-contracts [--check|--write]");
            process::exit(2);
        }
    }
}
