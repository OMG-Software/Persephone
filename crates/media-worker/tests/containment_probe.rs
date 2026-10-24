//! Containment probes with negative controls (ADR-0010, ADR-0011).
//!
//! Each case runs this test binary again as a child process. The child
//! either applies exactly the worker's production containment
//! (`persephone_media_worker::contain()`, after the launcher's part on Linux:
//! `no_new_privs`) or applies nothing, and then tries one thing:
//!
//! * open a socket, open `/etc/hosts`, or fork: with the containment, Linux
//!   kills the child with SIGSYS and macOS refuses with `EPERM`; without it,
//!   each one succeeds, which shows the probe can detect the difference;
//! * nothing but write to stdout and exit: this must still work with the
//!   containment, so the filter does not just kill everything.
//!
//! The harness is this file's own `main`, so no test framework threads run in
//! the children.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() {
    println!("containment probes: no Linux or macOS containment on this OS; nothing to run");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() -> std::process::ExitCode {
    probe::main()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod probe {
    use std::{
        env,
        io::Write,
        process::{Command, ExitCode, Stdio},
    };

    const CHILD: &str = "--probe-child";
    /// Child exit codes.
    const SUCCEEDED: i32 = 0;
    const REFUSED_EPERM: i32 = 10;
    const FAILED_OTHERWISE: i32 = 11;
    const NOT_CONTAINED: i32 = 12;
    /// prctl arguments as full words.
    #[cfg(target_os = "linux")]
    const ON: libc::c_ulong = 1;
    #[cfg(target_os = "linux")]
    const NONE: libc::c_ulong = 0;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Outcome {
        Succeeded,
        RefusedEperm,
        KilledBySigsys,
        Other,
    }

    pub fn main() -> ExitCode {
        let args: Vec<String> = env::args().skip(1).collect();
        if args.first().map(String::as_str) == Some(CHILD) && args.len() == 3 {
            child(&args[1], &args[2]);
        }
        let denied = if cfg!(target_os = "linux") {
            Outcome::KilledBySigsys
        } else {
            Outcome::RefusedEperm
        };
        let cases = [
            // GIVEN no containment WHEN the probe tries each action THEN it
            // succeeds (negative control).
            ("open", "socket", Outcome::Succeeded),
            ("open", "file", Outcome::Succeeded),
            ("open", "fork", Outcome::Succeeded),
            // GIVEN the production containment WHEN the probe tries each
            // action THEN it is killed or refused.
            ("contained", "socket", denied),
            ("contained", "file", denied),
            ("contained", "fork", denied),
            // GIVEN the production containment WHEN the probe only writes to
            // stdout and exits THEN that still works.
            ("contained", "nothing", Outcome::Succeeded),
        ];
        let mut failed = 0;
        for (policy, attempt, expected) in cases {
            let (actual, stdout) = run_child(policy, attempt);
            let pass = actual == expected && (attempt != "nothing" || stdout == b"ok");
            println!(
                "probe {policy}/{attempt}: expected {expected:?}, got {actual:?} ... {}",
                if pass { "ok" } else { "FAILED" }
            );
            if !pass {
                failed += 1;
            }
        }
        println!(
            "containment probes: {} passed; {failed} failed",
            cases.len() - failed
        );
        if failed == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }

    fn run_child(policy: &str, attempt: &str) -> (Outcome, Vec<u8>) {
        let output = Command::new(env::current_exe().unwrap())
            .args([CHILD, policy, attempt])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .output()
            .unwrap();
        use std::os::unix::process::ExitStatusExt;
        let outcome = match (output.status.code(), output.status.signal()) {
            (Some(SUCCEEDED), _) => Outcome::Succeeded,
            (Some(REFUSED_EPERM), _) => Outcome::RefusedEperm,
            (_, Some(libc::SIGSYS)) => Outcome::KilledBySigsys,
            _ => Outcome::Other,
        };
        if outcome == Outcome::Other {
            eprintln!("probe {policy}/{attempt}: status {:?}", output.status);
        }
        (outcome, output.stdout)
    }

    fn child(policy: &str, attempt: &str) -> ! {
        if policy == "contained" {
            // The launcher's part of the Linux containment; the broker sets
            // it in the worker before exec.
            #[cfg(target_os = "linux")]
            // SAFETY: prctl with integer arguments only.
            if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, ON, NONE, NONE, NONE) } != 0 {
                std::process::exit(NOT_CONTAINED);
            }
            if persephone_media_worker::contain() != 0 {
                std::process::exit(NOT_CONTAINED);
            }
        }
        let result = match attempt {
            // SAFETY: plain system calls; a created socket or process is
            // cleaned up by the exit below.
            "socket" => unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) },
            "file" => match std::fs::File::open("/etc/hosts") {
                Ok(_) => 0,
                Err(error) => {
                    errno_exit(error.raw_os_error().unwrap_or(0));
                }
            },
            // SAFETY: the forked child only exits; the parent reaps it.
            "fork" => unsafe {
                match libc::fork() {
                    0 => libc::_exit(0),
                    -1 => -1,
                    pid => {
                        let mut status = 0;
                        libc::waitpid(pid, &mut status, 0);
                        0
                    }
                }
            },
            "nothing" => {
                let mut stdout = std::io::stdout().lock();
                if stdout
                    .write_all(b"ok")
                    .and_then(|()| stdout.flush())
                    .is_err()
                {
                    std::process::exit(FAILED_OTHERWISE);
                }
                0
            }
            _ => std::process::exit(FAILED_OTHERWISE),
        };
        if result < 0 {
            errno_exit(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
        }
        std::process::exit(SUCCEEDED)
    }

    fn errno_exit(errno: i32) -> ! {
        std::process::exit(if errno == libc::EPERM {
            REFUSED_EPERM
        } else {
            FAILED_OTHERWISE
        })
    }
}
