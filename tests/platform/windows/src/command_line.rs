#![cfg(windows)]

use std::{ffi::OsStr, os::windows::ffi::OsStrExt};

/// Quotes one Windows command-line argument without a terminating NUL.
///
/// Uses the standard backslash/quote escaping rules. Executable arguments must
/// be valid executable paths, since Windows parses the first argument specially.
/// Panics if the argument contains a NUL, which cannot be passed to CreateProcessW.
pub(crate) fn quote(argument: &OsStr) -> Vec<u16> {
    const BACKSLASH: u16 = b'\\' as u16;
    const QUOTE: u16 = b'"' as u16;

    let argument: Vec<u16> = argument.encode_wide().collect();
    assert!(!argument.contains(&0), "command-line argument contains NUL");

    let mut quoted = vec![QUOTE];
    let mut backslashes = 0;
    for unit in argument {
        if unit == BACKSLASH {
            backslashes += 1;
            continue;
        }
        let escaped_backslashes = if unit == QUOTE {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        quoted.extend(std::iter::repeat_n(BACKSLASH, escaped_backslashes));
        quoted.push(unit);
        backslashes = 0;
    }
    // A trailing backslash run precedes our closing quote, so double it too.
    quoted.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2));
    quoted.push(QUOTE);
    quoted
}

#[cfg(test)]
mod tests {
    use super::quote;
    use std::{ffi::OsString, os::windows::ffi::OsStringExt, ptr, slice};

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn CommandLineToArgvW(command_line: *const u16, argument_count: *mut i32) -> *mut *mut u16;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    }

    struct ParsedArguments(*mut *mut u16);

    impl Drop for ParsedArguments {
        fn drop(&mut self) {
            // CommandLineToArgvW returns a single allocation for the whole list.
            unsafe { LocalFree(self.0.cast()) };
        }
    }

    fn assert_round_trip(arguments: &[Vec<u16>]) {
        let executable: Vec<u16> = r"C:\Program Files\Persephone\probe.exe"
            .encode_utf16()
            .collect();
        let mut expected = vec![executable];
        expected.extend_from_slice(arguments);
        // A following argument also detects an escaped closing quote swallowing
        // the command line after an argument with trailing backslashes.
        expected.push("following sentinel".encode_utf16().collect());

        let mut command_line = Vec::new();
        for argument in &expected {
            if !command_line.is_empty() {
                command_line.push(b' ' as u16);
            }
            command_line.extend(quote(&OsString::from_wide(argument)));
        }
        command_line.push(0);

        let mut count = 0;
        let parsed = unsafe { CommandLineToArgvW(command_line.as_ptr(), &mut count) };
        assert!(!parsed.is_null(), "CommandLineToArgvW failed");
        let parsed = ParsedArguments(parsed);
        assert_eq!(count as usize, expected.len());
        for (index, expected_argument) in expected.iter().enumerate() {
            let argument = unsafe { *parsed.0.add(index) };
            assert_ne!(argument, ptr::null_mut());
            let length = unsafe {
                (0..)
                    .take_while(|&offset| *argument.add(offset) != 0)
                    .count()
            };
            let actual = unsafe { slice::from_raw_parts(argument, length) };
            assert_eq!(actual, expected_argument, "argument {index}");
        }
    }

    #[test]
    fn preserves_empty_whitespace_unicode_and_path_arguments() {
        let arguments = [
            "",
            "plain",
            "two words",
            "\tleading and trailing\t",
            "Grüße 日本語 🦉",
            r"C:\directory with spaces\",
            r"C:\directory\\",
            "\"quoted\"",
            "\\\"quote after slash",
            "\\\\\"quote after two slashes",
            "a\"\"b",
        ]
        .map(|argument| argument.encode_utf16().collect());
        assert_round_trip(&arguments);
    }

    #[test]
    fn preserves_generated_quote_backslash_and_whitespace_combinations() {
        const ALPHABET: [u16; 5] = [
            b'a' as u16,
            b' ' as u16,
            b'\t' as u16,
            b'\\' as u16,
            b'"' as u16,
        ];
        for length in 0..=5u32 {
            for mut number in 0..ALPHABET.len().pow(length) {
                let mut argument = Vec::new();
                for _ in 0..length {
                    argument.push(ALPHABET[number % ALPHABET.len()]);
                    number /= ALPHABET.len();
                }
                assert_round_trip(&[argument]);
            }
        }
    }

    #[test]
    fn preserves_unpaired_utf16_surrogates() {
        assert_round_trip(&[
            vec![0xd800],
            vec![0xdc00],
            vec![0xd800, b'\\' as u16, b'"' as u16, 0xdc00],
        ]);
    }

    #[test]
    #[should_panic(expected = "command-line argument contains NUL")]
    fn rejects_embedded_nul() {
        quote(&OsString::from_wide(&[b'a' as u16, 0, b'b' as u16]));
    }
}
