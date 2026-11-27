//! Persephone's entry in the desktop's app menu, for a Linux AppImage
//! (ADR-0072).
//!
//! KWin finds a Wayland window's icon only through `<app ID>.desktop` in an
//! applications folder, and an AppImage that no tool installed has none.
//! When the user asks in Settings and answers yes in the core's own window,
//! the core writes `Persephone.desktop` and its icons for the running
//! AppImage under the user's data folder.
//!
//! This module holds the rules that do not touch the disk: which AppImage
//! paths an entry can name, the entry's exact text, and what an existing
//! file means. `platform/linux/app_menu.rs` checks the AppImage and does
//! the file work. Every other system is [`Unavailable`].

use persephone_contracts::AppMenuState;

/// The desktop file ID: the product name, which the Linux UI sets as its
/// program name, so it is each window's Wayland app ID. The packages
/// install a file of the same name.
pub const DESKTOP_FILE: &str = "Persephone.desktop";
/// The entry's icon name. It is not the packages' `persephone-ui`, so the
/// core never replaces a package's icons and removes only its own.
pub const ICON_NAME: &str = "persephone-appimage";
/// A line of the entry that tells a reader whose file it is. The whole
/// exact text, not this line, makes a file Persephone's own ([`is_ours`]).
pub const MARK: &str = "X-Persephone-AppImage=true";
/// Longest AppImage path, in bytes (Linux `PATH_MAX`).
pub const MAX_PATH_BYTES: usize = 4096;
/// Longest existing entry that is read, in bytes. Persephone's own entry is
/// much smaller, so a longer file is another program's.
pub const MAX_ENTRY_BYTES: u64 = 16 * 1024;

/// Why an add or a remove did not complete. Neither carries the cause: the
/// page gets a fixed error code only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppMenuError {
    /// Not started from a checked AppImage, or not Linux.
    Unavailable,
    /// The question, or writing or deleting a file, failed.
    Failed,
}

/// The AppImage and the data folder, as the core's question shows them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub appimage: String,
    pub data_home: String,
}

/// The app menu entry of this system.
pub trait AppMenu: Send + Sync {
    /// The state now.
    fn state(&self) -> AppMenuState;

    /// What the question shows; `None` when the state is `unavailable`.
    fn place(&self) -> Option<Place>;

    /// Writes the icons, then the entry, when the state is `absent` or
    /// `outdated`, and returns the new state. Any other state is returned
    /// as it is, and nothing is written. The caller asks the user first.
    fn add(&self) -> Result<AppMenuState, AppMenuError>;

    /// Removes the entry and the icons when the entry is Persephone's own,
    /// and returns the new state. Another program's file stays.
    fn remove(&self) -> Result<AppMenuState, AppMenuError>;
}

/// Every system but Linux, and a core built without an implementation.
pub struct Unavailable;

impl AppMenu for Unavailable {
    fn state(&self) -> AppMenuState {
        AppMenuState::Unavailable
    }

    fn place(&self) -> Option<Place> {
        None
    }

    fn add(&self) -> Result<AppMenuState, AppMenuError> {
        Err(AppMenuError::Unavailable)
    }

    fn remove(&self) -> Result<AppMenuState, AppMenuError> {
        Err(AppMenuError::Unavailable)
    }
}

/// The platform's app menu: the XDG one on Linux, [`Unavailable`] elsewhere.
pub fn platform() -> std::sync::Arc<dyn AppMenu> {
    #[cfg(target_os = "linux")]
    {
        std::sync::Arc::new(crate::platform::linux::app_menu::XdgAppMenu)
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::sync::Arc::new(Unavailable)
    }
}

/// Whether an entry can name this AppImage path, and the question can show
/// it as it is stored: absolute, at most [`MAX_PATH_BYTES`], with no control
/// character (no line break can start a new key), no bidirectional
/// formatting character, and no white space at its end, which some desktop
/// file readers remove.
pub fn nameable(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= MAX_PATH_BYTES
        && !path
            .chars()
            .any(|c| c.is_control() || crate::views::is_bidi_control(c))
        && !path.ends_with(char::is_whitespace)
}

/// The whole entry for the AppImage at `appimage`; `None` when the path is
/// not [`nameable`].
pub fn entry(appimage: &str) -> Option<String> {
    if !nameable(appimage) {
        return None;
    }
    Some(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Persephone\n\
         Comment=Matrix desktop client\n\
         Exec={exec}\n\
         TryExec={try_exec}\n\
         Icon={ICON_NAME}\n\
         StartupWMClass=Persephone\n\
         Categories=Network;\n\
         Terminal=false\n\
         {MARK}\n",
        exec = string_value(&exec_argument(appimage)),
        try_exec = string_value(appimage),
    ))
}

/// One `Exec` argument, quoted as the Desktop Entry Specification asks: in
/// double quotes, with `"`, `` ` ``, `$` and `\` after a backslash, and
/// with `%` doubled, because one `%` starts a field code. Readers expand
/// field codes before they split the arguments.
fn exec_argument(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// A value of type string: each backslash is written as `\\`. Readers undo
/// this first. [`nameable`] refused the control characters that would
/// need `\n`, `\t` or `\r`.
fn string_value(text: &str) -> String {
    text.replace('\\', "\\\\")
}

/// Whether an entry's bytes are exactly the text that [`entry`] makes for
/// the path in the entry's own `TryExec` line. The mark line alone is not
/// enough: a file with any other change is another program's, or one the
/// user edited, and is never replaced or removed.
pub fn is_ours(content: &[u8]) -> bool {
    std::str::from_utf8(content).is_ok_and(|text| {
        text.lines()
            .find_map(|line| line.strip_prefix("TryExec="))
            .and_then(unescape_string)
            .and_then(|path| entry(&path))
            .is_some_and(|expected| expected == text)
    })
}

/// Undoes [`string_value`]: `\\` is one backslash. Any other escape is not
/// one that Persephone writes.
fn unescape_string(value: &str) -> Option<String> {
    let mut text = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if chars.next()? != '\\' {
                return None;
            }
            text.push('\\');
        } else {
            text.push(c);
        }
    }
    Some(text)
}

/// What is at a path that Persephone may write: the user's
/// `Persephone.desktop`, or one of its icons.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    Missing,
    /// A regular file within the read limit, read whole.
    Content(Vec<u8>),
    /// A link, a folder, a larger or unreadable file: another program's.
    Other,
}

/// The state for the user's file and whether a data folder of the system
/// has a `Persephone.desktop` too. The user's file is looked at first: a
/// user entry of Persephone's own is reported (and can be removed) also
/// while a package's entry exists, which it then hides.
pub fn state(user: &Found, provided_elsewhere: bool, appimage: &str) -> AppMenuState {
    match user {
        Found::Content(content) if is_ours(content) => {
            if entry(appimage).is_some_and(|expected| expected.as_bytes() == content.as_slice()) {
                AppMenuState::Added
            } else {
                AppMenuState::Outdated
            }
        }
        Found::Content(_) | Found::Other => AppMenuState::Provided,
        Found::Missing if provided_elsewhere => AppMenuState::Provided,
        Found::Missing => AppMenuState::Absent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "/home/example/Applications/Persephone.AppImage";

    fn exec_line(path: &str) -> String {
        let text = entry(path).unwrap();
        text.lines()
            .find_map(|line| line.strip_prefix("Exec="))
            .unwrap()
            .to_owned()
    }

    /// Undoes the three levels a reader applies, in its order: the string
    /// escapes, the `%%` field code, then the double quotes and their
    /// backslashes. The reader's own steps are not run here, so this
    /// checks the encoding against the specification's rules.
    fn read_back(value: &str) -> String {
        let mut unescaped = String::new();
        let mut chars = value.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next().unwrap() {
                    '\\' => unescaped.push('\\'),
                    other => panic!("unexpected string escape \\{other}"),
                }
            } else {
                unescaped.push(c);
            }
        }
        let mut coded = String::new();
        let mut chars = unescaped.chars();
        while let Some(c) = chars.next() {
            if c == '%' {
                assert_eq!(chars.next(), Some('%'), "a lone % is a field code");
                coded.push('%');
            } else {
                coded.push(c);
            }
        }
        let inner = coded
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("one quoted argument");
        let mut argument = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    let next = chars.next().unwrap();
                    assert!(matches!(next, '"' | '`' | '$' | '\\'), "escaped {next}");
                    argument.push(next);
                }
                '"' | '`' | '$' => panic!("unescaped {c} inside the quotes"),
                _ => argument.push(c),
            }
        }
        argument
    }

    /// GIVEN a plain AppImage path WHEN the entry is made THEN it is the
    /// exact fixed text, with the path quoted in `Exec` and plain in
    /// `TryExec`, the own icon name, the product name as `StartupWMClass`,
    /// and the mark as the last line.
    #[test]
    fn given_a_plain_path_when_the_entry_is_made_then_it_is_the_fixed_text() {
        assert_eq!(
            entry(APP).unwrap(),
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Persephone\n\
             Comment=Matrix desktop client\n\
             Exec=\"/home/example/Applications/Persephone.AppImage\"\n\
             TryExec=/home/example/Applications/Persephone.AppImage\n\
             Icon=persephone-appimage\n\
             StartupWMClass=Persephone\n\
             Categories=Network;\n\
             Terminal=false\n\
             X-Persephone-AppImage=true\n"
        );
    }

    /// GIVEN paths with spaces, shell characters, quotes, back quotes,
    /// dollar signs, backslashes, percent signs and non-ASCII text WHEN the
    /// entry is made THEN `Exec` holds one quoted argument that reads back
    /// as exactly the path, so no part of a file name becomes another
    /// argument, a field code or a command.
    #[test]
    fn given_hostile_file_names_when_quoted_then_exec_reads_back_as_the_path() {
        for path in [
            APP,
            "/home/example/Downloads/Persephone_0.3.0_amd64-unsigned (1).AppImage",
            "/tmp/a b/x;rm -rf ~ &|<>*?#()'.AppImage",
            "/tmp/\"quoted\"/x.AppImage",
            "/tmp/`id`/$(id)/${HOME}.AppImage",
            "/tmp/back\\slash\\\\/x.AppImage",
            "/tmp/%f %U %%/x.AppImage",
            "/tmp/caf\u{e9}/\u{65e5}\u{672c}/x.AppImage",
        ] {
            let exec = exec_line(path);
            assert_eq!(read_back(&exec), path, "{exec}");
        }
        assert_eq!(
            exec_line("/a$b"),
            "\"/a\\\\$b\"",
            "the specification's form of $"
        );
        assert_eq!(
            exec_line("/a\\b"),
            "\"/a\\\\\\\\b\"",
            "four backslashes for one"
        );
        assert_eq!(exec_line("/a%b"), "\"/a%%b\"");
    }

    /// GIVEN a path with a backslash WHEN the entry is made THEN `TryExec`
    /// has only the string escape, since it is not a command line.
    #[test]
    fn given_a_backslash_when_the_entry_is_made_then_try_exec_has_only_the_string_escape() {
        let text = entry("/tmp/a\\b \"c\"%.AppImage").unwrap();
        assert!(
            text.contains("\nTryExec=/tmp/a\\\\b \"c\"%.AppImage\n"),
            "{text}"
        );
    }

    /// GIVEN paths with a line break, a tab, another control character, a
    /// bidirectional override or isolate, white space at the end, no
    /// leading slash, or more than the limit WHEN checked THEN no entry is
    /// made, so no path can add a key or show reordered text.
    #[test]
    fn given_unsafe_paths_when_checked_then_no_entry_is_made() {
        let long = format!("/{}", "a".repeat(MAX_PATH_BYTES));
        for path in [
            "/tmp/x.AppImage\nExec=/bin/sh",
            "/tmp/x\rTerminal=true.AppImage",
            "/tmp/x\t.AppImage",
            "/tmp/x\u{0}.AppImage",
            "/tmp/x\u{7f}.AppImage",
            "/tmp/x\u{85}.AppImage",
            "/tmp/evil\u{202e}gamIppA.exe",
            "/tmp/\u{2067}x\u{2069}.AppImage",
            "/tmp/\u{200f}x.AppImage",
            "/tmp/x.AppImage ",
            "/tmp/x.AppImage\u{a0}",
            "relative/x.AppImage",
            "",
            long.as_str(),
        ] {
            assert!(!nameable(path), "{path:?}");
            assert_eq!(entry(path), None, "{path:?}");
        }
        let longest = format!("/{}", "a".repeat(MAX_PATH_BYTES - 1));
        assert!(nameable(&longest));
        assert!(nameable("/tmp/ x.AppImage"), "inner spaces are quoted");
    }

    /// GIVEN Persephone's own entry for this path and for another path WHEN
    /// the state is read THEN it is `added` and `outdated`. GIVEN either
    /// with any changed, added or removed line, or other line ends, THEN it
    /// is `provided`: the mark line alone does not make a file Persephone's.
    #[test]
    fn given_own_entries_when_read_then_only_the_exact_text_is_ours() {
        let own = entry(APP).unwrap();
        assert_eq!(
            state(&Found::Content(own.clone().into_bytes()), false, APP),
            AppMenuState::Added
        );
        let other = entry("/opt/Persephone.AppImage").unwrap();
        assert_eq!(
            state(&Found::Content(other.clone().into_bytes()), false, APP),
            AppMenuState::Outdated
        );
        for edited in [
            own.replace("Terminal=false", "Terminal=true"),
            own.replace("Exec=\"", "Exec=env X=1 \""),
            other.replace("TryExec=/opt", "TryExec=/srv"),
            format!("{own}Hidden=true\n"),
            own.replace("Comment=Matrix desktop client\n", ""),
            own.replace('\n', "\r\n"),
            format!("[Desktop Entry]\nName=Persephone\nExec=x\n{MARK}\n"),
        ] {
            assert!(!is_ours(edited.as_bytes()), "{edited}");
            assert_eq!(
                state(&Found::Content(edited.into_bytes()), false, APP),
                AppMenuState::Provided
            );
        }
    }

    /// GIVEN the own entry for paths with backslashes and other hostile
    /// characters WHEN read back THEN it is Persephone's; GIVEN a `TryExec`
    /// with an escape that Persephone never writes THEN it is not.
    #[test]
    fn given_hostile_paths_when_the_own_entry_is_read_back_then_it_is_ours() {
        for path in [
            "/tmp/back\\slash\\\\/x.AppImage",
            "/tmp/\"q\"/`id`/$x/%f.AppImage",
            "/tmp/caf\u{e9}/x.AppImage",
        ] {
            assert!(is_ours(entry(path).unwrap().as_bytes()), "{path}");
        }
        let own = entry(APP).unwrap();
        let escaped = own.replace("TryExec=/home", "TryExec=\\s/home");
        assert!(!is_ours(escaped.as_bytes()));
        assert_eq!(unescape_string("a\\"), None, "a lone backslash at the end");
    }

    /// GIVEN a package's entry in a system folder, or a user file without
    /// the mark, a link or a large file WHEN the state is read THEN it is
    /// `provided`, so nothing is written over it. GIVEN Persephone's own
    /// user entry while a package's exists THEN it is still reported, so
    /// the user can remove the entry that hides the package's.
    #[test]
    fn given_other_entries_when_read_then_they_are_provided() {
        assert_eq!(state(&Found::Missing, false, APP), AppMenuState::Absent);
        assert_eq!(state(&Found::Missing, true, APP), AppMenuState::Provided);
        assert_eq!(state(&Found::Other, false, APP), AppMenuState::Provided);
        let package = b"[Desktop Entry]\nName=Persephone\nExec=persephone-ui\n".to_vec();
        assert_eq!(
            state(&Found::Content(package), false, APP),
            AppMenuState::Provided
        );
        let mark_in_text = format!("[Desktop Entry]\nComment={MARK}\n").into_bytes();
        assert_eq!(
            state(&Found::Content(mark_in_text), false, APP),
            AppMenuState::Provided,
            "the mark must be a line of its own"
        );
        assert_eq!(
            state(&Found::Content(vec![0xff, 0xfe]), false, APP),
            AppMenuState::Provided
        );
        let own = entry(APP).unwrap().into_bytes();
        assert_eq!(state(&Found::Content(own), true, APP), AppMenuState::Added);
    }

    /// GIVEN any system's fallback THEN it is unavailable and writes
    /// nothing.
    #[test]
    fn given_the_unavailable_menu_then_it_refuses_every_change() {
        assert_eq!(Unavailable.state(), AppMenuState::Unavailable);
        assert_eq!(Unavailable.place(), None);
        assert_eq!(Unavailable.add(), Err(AppMenuError::Unavailable));
        assert_eq!(Unavailable.remove(), Err(AppMenuError::Unavailable));
    }
}
