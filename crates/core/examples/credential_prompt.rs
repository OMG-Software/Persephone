//! Shows the native credential prompt once and reports what came back, for
//! manual checks. Prints lengths and yes/no comparisons only, never entries.
//!
//! `cargo run -p persephone-core --example credential_prompt -- <host>`
//!
//! With `--compare <harness profile.json> <server name>`, also says whether
//! the entries equal that disposable account's username and password, to tell
//! a prompt or unpacking fault from a mistyped or mis-pasted entry.

use persephone_core::credentials;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let host = args
        .first()
        .cloned()
        .unwrap_or_else(|| "matrix.example.org".into());
    let entered = match credentials::platform().ask(&host) {
        Ok(entered) => entered,
        Err(error) => {
            println!("error {error:?}");
            return;
        }
    };
    let password = entered.password.as_str();
    println!(
        "ok username_chars={} password_chars={} protected_form={}",
        entered.username.chars().count(),
        password.chars().count(),
        password.starts_with("@@")
    );
    if let [_, flag, profile, server] = args.as_slice()
        && flag == "--compare"
    {
        let entries: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(profile).expect("profile readable"))
                .expect("profile is JSON");
        let entry = entries
            .as_array()
            .and_then(|list| list.iter().find(|e| e["name"] == server.as_str()))
            .expect("server in profile");
        let expected = entry["password"].as_str().unwrap_or_default();
        let trimmed = password.trim().trim_matches('"').trim_matches('\'');
        println!(
            "username_matches={} password_matches={} matches_after_trimming_spaces_or_quotes={} expected_chars={}",
            entered.username.as_str() == entry["username"].as_str().unwrap_or_default(),
            password == expected,
            trimmed == expected,
            expected.chars().count()
        );
        println!(
            "password_equals_username={} is_start_of_expected={} is_end_of_expected={}",
            password == entered.username.as_str(),
            !password.is_empty() && expected.starts_with(password),
            !password.is_empty() && expected.ends_with(password)
        );
    }
}
