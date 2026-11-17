//! A report of the code mapped into Persephone's processes on Linux
//! (ADR-0047).
//!
//! Linux has no signatures for libraries, so the scanner judges each file
//! mapped executable by who can change it:
//!
//! * Expected, not reported: Persephone's own executables in the core's
//!   folder; any file on the read-only file system the core runs from (an
//!   AppImage); and a file in a system folder ([`SYSTEM_DIRS`]) that root
//!   owns and only root can change, in folders that only root can change. A
//!   sticky folder, such as `/nix/store`, counts when root owns the entry.
//! * Reported, with the most serious reason first: a file named in
//!   `LD_PRELOAD`, `LD_AUDIT` or `/etc/ld.so.preload`, even a system file; in
//!   the core only, executable memory with no file behind it (the core has
//!   no JIT, so this can be injected code); a file deleted since it was
//!   loaded; a file that a user other than root can change; any other file.
//!
//! The web view's JIT makes fileless executable memory normal there, so only
//! the core is checked for it. A process in another mount namespace is not
//! judged, because its paths name other files: the scan is then incomplete.
//!
//! The scanner reads `/proc/<pid>/maps` and `/proc/<pid>/environ` of the core
//! and its own child processes. It makes no network request, shows no UI and
//! never logs a path, because a path can hold the user name.

use crate::modules::Found;
use persephone_contracts::{ModuleProcess, ModuleReason, ModuleSignature};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CString, OsStr},
    fs::{self, File},
    io::{self, Read},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

/// The most bytes read from one `maps` file. A larger one is incomplete.
const MAX_MAPS_BYTES: u64 = 8 * 1024 * 1024;
/// The most bytes read from one `environ` file.
const MAX_ENVIRON_BYTES: u64 = 1024 * 1024;
/// The most bytes read from `/etc/ld.so.preload`.
const MAX_PRELOAD_FILE_BYTES: u64 = 64 * 1024;
/// The most distinct files judged in one process. More is incomplete.
const MAX_FILES: usize = 8192;
/// The most folders walked above one file.
const MAX_DEPTH: usize = 64;
/// Folders that hold the system's libraries. A file here is expected only
/// when only root can change it and every folder above it.
pub const SYSTEM_DIRS: [&str; 7] = [
    "/usr/",
    "/lib/",
    "/lib32/",
    "/lib64/",
    "/libx32/",
    "/nix/store/",
    "/gnu/store/",
];
/// Persephone's own executables, expected in the core's folder.
const OWN_NAMES: [&str; 3] = ["persephone", "persephone-ui", "persephone-media-worker"];
/// Executable pages the kernel provides; not code from a file.
const KERNEL_PAGES: [&[u8]; 3] = [b"[vdso]", b"[vsyscall]", b"[uprobes]"];
/// The name of executable memory with no name at all.
pub const ANONYMOUS: &str = "[anonymous]";
const DELETED: &[u8] = b" (deleted)";

/// One process to scan.
pub struct ScanTarget {
    pub pid: u32,
    pub process: ModuleProcess,
}

/// The result of one scan.
pub struct ScanResult {
    pub modules: Vec<Found>,
    /// A target process, or some of its mappings or its environment, could
    /// not be read or judged. A process that has exited is not counted.
    pub incomplete: bool,
}

/// One executable mapping, as `/proc/<pid>/maps` names it.
#[derive(Debug, PartialEq, Eq)]
pub enum Mapping<'a> {
    /// A file, and whether the kernel marks it deleted.
    File { path: &'a [u8], deleted: bool },
    /// Executable memory with no file: anonymous, a memfd or a named region
    /// such as `[heap]`.
    NoFile(&'a [u8]),
}

/// The executable mappings in a `maps` text, in order. The kernel pages are
/// left out. A line that does not parse is skipped.
pub fn executable_mappings(maps: &[u8]) -> Vec<Mapping<'_>> {
    let mut found = Vec::new();
    for line in maps.split(|&b| b == b'\n') {
        // address perms offset dev inode [path]; the path may hold spaces.
        let mut rest = line;
        let mut fields = [&b""[..]; 5];
        let mut ok = true;
        for field in &mut fields {
            rest = trim_start(rest);
            let end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
            if end == 0 {
                ok = false;
                break;
            }
            *field = &rest[..end];
            rest = &rest[end..];
        }
        if !ok || fields[1].get(2) != Some(&b'x') {
            continue;
        }
        let path = trim_start(rest);
        if path.is_empty() {
            found.push(Mapping::NoFile(ANONYMOUS.as_bytes()));
        } else if path.starts_with(b"[") {
            if !KERNEL_PAGES.contains(&path) {
                found.push(Mapping::NoFile(path));
            }
        } else if path.starts_with(b"/memfd:") {
            found.push(Mapping::NoFile(path));
        } else if let Some(stripped) = path.strip_suffix(DELETED) {
            found.push(Mapping::File {
                path: stripped,
                deleted: true,
            });
        } else {
            found.push(Mapping::File {
                path,
                deleted: false,
            });
        }
    }
    found
}

fn trim_start(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|&b| b != b' ').unwrap_or(bytes.len());
    &bytes[start..]
}

/// The file names that `LD_PRELOAD` and `LD_AUDIT` name in an `environ`
/// text (NUL-separated `NAME=value` entries).
pub fn environ_preloads(environ: &[u8]) -> BTreeSet<Vec<u8>> {
    let mut names = BTreeSet::new();
    for entry in environ.split(|&b| b == 0) {
        for key in [&b"LD_PRELOAD="[..], b"LD_AUDIT="] {
            if let Some(value) = entry.strip_prefix(key) {
                names.extend(preload_names(value));
            }
        }
    }
    names
}

/// The file names in a preload list: entries are separated by spaces, tabs,
/// new lines or colons, and may be paths or bare names.
pub fn preload_names(list: &[u8]) -> BTreeSet<Vec<u8>> {
    list.split(|&b| matches!(b, b' ' | b'\t' | b'\n' | b':'))
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| entry.rsplit(|&b| b == b'/').next())
        .filter(|name| !name.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// Whether only root can change an entry with this owner and mode. A folder
/// that others can write to still counts when it is sticky, because then
/// only the owner of an entry, root here, can remove or rename it.
pub fn root_only(uid: u32, mode: u32, is_dir: bool) -> bool {
    uid == 0 && (mode & 0o022 == 0 || (is_dir && mode & 0o1000 != 0))
}

/// Whether the path is in a system folder.
pub fn in_system_dir(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    SYSTEM_DIRS
        .iter()
        .any(|dir| bytes.starts_with(dir.as_bytes()))
}

/// Whether only root can change the file and every folder above it. An
/// entry that cannot be read is not shown to be safe.
fn only_root_can_change(path: &Path) -> io::Result<bool> {
    let meta = fs::metadata(path)?;
    if !root_only(meta.uid(), meta.mode(), meta.is_dir()) {
        return Ok(false);
    }
    let mut depth = 0;
    let mut dir = path.parent();
    while let Some(folder) = dir {
        depth += 1;
        if depth > MAX_DEPTH {
            return Ok(false);
        }
        let meta = fs::metadata(folder)?;
        if !meta.is_dir() || !root_only(meta.uid(), meta.mode(), true) {
            return Ok(false);
        }
        dir = folder.parent();
    }
    Ok(true)
}

/// What the core knows about itself, read once.
struct Own {
    /// The folder of the core's executable.
    dir: Option<PathBuf>,
    /// The device of the core's executable, when its file system is
    /// mounted read-only (an AppImage).
    image_device: Option<u64>,
    /// The core's mount namespace.
    namespace: Option<PathBuf>,
}

impl Own {
    fn read() -> Self {
        let exe = fs::read_link("/proc/self/exe").ok();
        let image_device = exe
            .as_deref()
            .filter(|exe| read_only_mount(exe))
            .and_then(|exe| fs::metadata(exe).ok())
            .map(|meta| meta.dev());
        Self {
            dir: exe.as_deref().and_then(Path::parent).map(Path::to_path_buf),
            image_device,
            namespace: fs::read_link("/proc/self/ns/mnt").ok(),
        }
    }

    /// Persephone's own: one of its executables in the core's folder, or any
    /// file on the read-only image the core runs from.
    fn owns(&self, path: &Path, device: Option<u64>) -> bool {
        let own_exe = path.parent() == self.dir.as_deref()
            && path
                .file_name()
                .is_some_and(|name| OWN_NAMES.iter().any(|own| name == OsStr::new(own)));
        own_exe || (self.image_device.is_some() && device == self.image_device)
    }
}

/// Whether the file system holding `path` is mounted read-only.
fn read_only_mount(path: &Path) -> bool {
    let Ok(text) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: a NUL-terminated path and a buffer of the right type.
    let ok = unsafe { libc::statvfs(text.as_ptr(), stat.as_mut_ptr()) } == 0;
    // SAFETY: statvfs filled the buffer when it returned 0; it was zeroed.
    ok && unsafe { stat.assume_init() }.f_flag & libc::ST_RDONLY != 0
}

/// Why a file is reported, or `None` when it is expected. `preloaded` names
/// the file names the process was told to preload.
fn judge(
    own: &Own,
    path: &Path,
    deleted: bool,
    preloaded: &BTreeSet<Vec<u8>>,
) -> Option<ModuleReason> {
    let meta = fs::metadata(path);
    let device = meta.as_ref().ok().map(MetadataExt::dev);
    if !deleted && own.owns(path, device) {
        return None;
    }
    let name = path.file_name().map(OsStrExt::as_bytes).unwrap_or_default();
    if preloaded.contains(name) {
        return Some(ModuleReason::Preloaded);
    }
    match meta {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Some(ModuleReason::NoFile);
        }
        Err(_) => return Some(ModuleReason::OutsideSystem),
    }
    if deleted {
        // The mapped file is gone. A file now at its path that is expected
        // is an update in place; anything else leaves the code without a
        // file to judge.
        return judge(own, path, false, &BTreeSet::new()).map(|_| ModuleReason::NoFile);
    }
    match only_root_can_change(path) {
        Ok(true) if in_system_dir(path) => None,
        Ok(true) => Some(ModuleReason::OutsideSystem),
        Ok(false) => Some(ModuleReason::UserWritable),
        Err(_) => Some(ModuleReason::OutsideSystem),
    }
}

/// The modules read from one process.
enum Listed {
    /// The process has exited.
    Gone,
    /// It could not be read or judged at all.
    Unreadable,
    Read {
        modules: Vec<Found>,
        complete: bool,
    },
}

/// Lists the code in Persephone's processes that is not expected.
pub struct Scanner {
    own: Own,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    pub fn new() -> Self {
        Self { own: Own::read() }
    }

    /// Lists the unexpected code in the targets, checking `stop` before each
    /// process and between files. `None` when it stopped: the partial result
    /// is dropped. Never panics on what it reads.
    pub fn scan_until(
        &mut self,
        targets: &[ScanTarget],
        stop: &dyn Fn() -> bool,
    ) -> Option<ScanResult> {
        let (system_preloads, mut incomplete) =
            match read_limited(Path::new("/etc/ld.so.preload"), MAX_PRELOAD_FILE_BYTES) {
                Ok((text, false)) => (preload_names(&text), false),
                Ok((text, true)) => (preload_names(&text), true),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (BTreeSet::new(), false),
                Err(_) => (BTreeSet::new(), true),
            };
        let mut modules = Vec::new();
        for target in targets {
            if stop() {
                return None;
            }
            match self.list(target, &system_preloads, stop)? {
                Listed::Gone => {}
                Listed::Unreadable => incomplete = true,
                Listed::Read {
                    modules: found,
                    complete,
                } => {
                    modules.extend(found);
                    incomplete |= !complete;
                }
            }
        }
        Some(ScanResult {
            modules,
            incomplete,
        })
    }

    fn list(
        &self,
        target: &ScanTarget,
        system_preloads: &BTreeSet<Vec<u8>>,
        stop: &dyn Fn() -> bool,
    ) -> Option<Listed> {
        let proc_dir = PathBuf::from(format!("/proc/{}", target.pid));
        let own_process = target.pid == std::process::id();
        if !own_process {
            match fs::read_link(proc_dir.join("ns/mnt")) {
                Ok(namespace) if Some(&namespace) == self.own.namespace.as_ref() => {}
                Ok(_) => return Some(Listed::Unreadable),
                Err(error) if gone(&error) => return Some(Listed::Gone),
                Err(_) => return Some(Listed::Unreadable),
            }
        }
        let (maps, cut) = match read_limited(&proc_dir.join("maps"), MAX_MAPS_BYTES) {
            Ok(read) => read,
            Err(error) if gone(&error) => return Some(Listed::Gone),
            Err(_) => return Some(Listed::Unreadable),
        };
        let mut complete = !cut;
        let mut preloaded = system_preloads.clone();
        match read_limited(&proc_dir.join("environ"), MAX_ENVIRON_BYTES) {
            Ok((environ, cut)) => {
                preloaded.extend(environ_preloads(&environ));
                complete &= !cut;
            }
            Err(error) if gone(&error) => return Some(Listed::Gone),
            Err(_) => complete = false,
        }
        let mut files: BTreeMap<PathBuf, bool> = BTreeMap::new();
        let mut no_file: BTreeSet<Vec<u8>> = BTreeSet::new();
        for mapping in executable_mappings(&maps) {
            match mapping {
                Mapping::File { path, deleted } => {
                    let path = PathBuf::from(OsStr::from_bytes(path));
                    if files.len() >= MAX_FILES && !files.contains_key(&path) {
                        complete = false;
                        continue;
                    }
                    *files.entry(path).or_insert(deleted) |= deleted;
                }
                // The core has no JIT; the web view's makes this normal.
                Mapping::NoFile(name) if target.process == ModuleProcess::Core => {
                    no_file.insert(name.to_vec());
                }
                Mapping::NoFile(_) => {}
            }
        }
        let mut modules = Vec::new();
        for (index, (path, deleted)) in files.into_iter().enumerate() {
            if index % 64 == 0 && stop() {
                return None;
            }
            if let Some(reason) = judge(&self.own, &path, deleted, &preloaded) {
                modules.push(found(path, target.process, reason));
            }
        }
        for name in no_file {
            modules.push(found(
                PathBuf::from(OsStr::from_bytes(&name)),
                target.process,
                ModuleReason::NoFile,
            ));
        }
        Some(Listed::Read { modules, complete })
    }
}

fn found(path: PathBuf, process: ModuleProcess, reason: ModuleReason) -> Found {
    Found {
        path,
        process,
        signature: ModuleSignature::NotChecked,
        signer: None,
        reason,
    }
}

/// Whether an error means the process has exited.
fn gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

/// At most `limit` bytes of a file, and whether it held more.
fn read_limited(path: &Path, limit: u64) -> io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    let cut = bytes.len() as u64 > limit;
    if cut {
        // Keep whole lines only.
        bytes.truncate(limit as usize);
        let end = bytes
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |at| at + 1);
        bytes.truncate(end);
    }
    Ok((bytes, cut))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// GIVEN `maps` lines of each kind WHEN parsed THEN only executable
    /// mappings are kept, paths keep their spaces, the kernel pages are left
    /// out, deleted files are marked, and memory with no file is named.
    #[test]
    fn given_maps_lines_then_only_executable_code_is_kept_and_named() {
        let maps = b"\
55d0a0000000-55d0a0001000 r--p 00000000 08:01 1234 /usr/bin/persephone
55d0a0001000-55d0a0002000 r-xp 00001000 08:01 1234 /usr/bin/persephone
7f0000000000-7f0000001000 r-xp 00000000 08:01 99 /home/a/My Libs/hook one.so
7f0000001000-7f0000002000 r-xp 00000000 08:01 98 /usr/lib/libold.so (deleted)
7f0000002000-7f0000003000 rwxp 00000000 00:00 0
7f0000003000-7f0000004000 r-xp 00000000 00:01 7 /memfd:jit (deleted)
7f0000004000-7f0000005000 r-xp 00000000 00:00 0 [vdso]
7f0000005000-7f0000006000 rwxp 00000000 00:00 0 [heap]
ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0 [vsyscall]
garbage
7f0000006000-7f0000007000 rw-p 00000000 08:01 97 /usr/lib/libdata.so
";
        assert_eq!(
            executable_mappings(maps),
            vec![
                Mapping::File {
                    path: b"/usr/bin/persephone",
                    deleted: false
                },
                Mapping::File {
                    path: b"/home/a/My Libs/hook one.so",
                    deleted: false
                },
                Mapping::File {
                    path: b"/usr/lib/libold.so",
                    deleted: true
                },
                Mapping::NoFile(ANONYMOUS.as_bytes()),
                Mapping::NoFile(b"/memfd:jit (deleted)"),
                Mapping::NoFile(b"[heap]"),
            ]
        );
    }

    /// GIVEN an environment WHEN read THEN the file names of `LD_PRELOAD`
    /// and `LD_AUDIT` are found, as paths or bare names, with any separator;
    /// a similar name is not.
    #[test]
    fn given_an_environment_then_preloaded_names_are_found() {
        let environ = b"HOME=/home/a\0LD_PRELOAD=/opt/x/libhook.so:libtwo.so  /lib/three.so\0\
LD_AUDIT=/tmp/audit.so\0NOT_LD_PRELOAD=/tmp/no.so\0LD_PRELOADX=/tmp/no2.so\0";
        let set = |names: &[&[u8]]| -> BTreeSet<Vec<u8>> {
            names.iter().map(|name| name.to_vec()).collect()
        };
        assert_eq!(
            environ_preloads(environ),
            set(&[b"libhook.so", b"libtwo.so", b"three.so", b"audit.so"])
        );
        // /etc/ld.so.preload has no comments: every word is a name.
        assert_eq!(
            preload_names(b"# not a comment\n/lib/a.so\n"),
            set(&[b"#", b"not", b"a", b"comment", b"a.so"])
        );
    }

    /// GIVEN owners and modes WHEN judged THEN only root-owned entries that
    /// group and others cannot write count, and a writable folder counts
    /// only when it is sticky; a sticky file does not.
    #[test]
    fn given_owners_and_modes_then_only_root_controlled_entries_count() {
        assert!(root_only(0, 0o100755, false));
        assert!(root_only(0, 0o40755, true));
        assert!(!root_only(1000, 0o100755, false), "a user's file");
        assert!(!root_only(0, 0o100775, false), "group can write");
        assert!(!root_only(0, 0o100757, false), "others can write");
        assert!(root_only(0, 0o41777, true), "sticky /tmp-like folder");
        assert!(root_only(0, 0o41775, true), "sticky /nix/store-like folder");
        assert!(!root_only(0, 0o101777, false), "a sticky file is not safe");
        assert!(
            !root_only(0, 0o40777, true),
            "a writable folder without sticky"
        );
        assert!(in_system_dir(Path::new(
            "/usr/lib/x86_64-linux-gnu/libc.so.6"
        )));
        assert!(in_system_dir(Path::new(
            "/nix/store/abc-glibc/lib/libc.so.6"
        )));
        assert!(!in_system_dir(Path::new("/usrlocal/lib/libx.so")));
        assert!(!in_system_dir(Path::new("/opt/vendor/lib/libx.so")));
        assert!(!in_system_dir(Path::new("/home/a/lib/libx.so")));
    }

    fn scan(pid: u32, process: ModuleProcess) -> ScanResult {
        Scanner::new()
            .scan_until(&[ScanTarget { pid, process }], &|| false)
            .expect("not stopped")
    }

    /// The C library this test process has mapped, from its own `maps`.
    fn mapped_libc() -> PathBuf {
        let maps = fs::read("/proc/self/maps").unwrap();
        executable_mappings(&maps)
            .into_iter()
            .find_map(|mapping| match mapping {
                Mapping::File { path, .. }
                    if path
                        .rsplit(|&b| b == b'/')
                        .next()
                        .is_some_and(|name| name.starts_with(b"libc.so")) =>
                {
                    Some(PathBuf::from(OsStr::from_bytes(path)))
                }
                _ => None,
            })
            .expect("the test process maps the C library")
    }

    /// GIVEN this test process WHEN scanned as the core THEN its system C
    /// library is not reported, and the test executable, which a user can
    /// change and which is not one of Persephone's own names, is.
    #[test]
    fn given_this_process_then_system_libraries_pass_and_a_user_file_is_reported() {
        let libc = mapped_libc();
        let result = scan(std::process::id(), ModuleProcess::Core);
        assert!(
            !result.modules.iter().any(|m| m.path == libc),
            "the system C library is expected"
        );
        let exe = fs::read_link("/proc/self/exe").unwrap();
        let only_root = only_root_can_change(&exe).unwrap_or(false);
        let row = result.modules.iter().find(|m| m.path == exe);
        if only_root {
            // A test binary installed by root (not the usual case).
            assert!(row.is_none_or(|m| m.reason == ModuleReason::OutsideSystem));
        } else {
            assert_eq!(row.map(|m| m.reason), Some(ModuleReason::UserWritable));
        }
        assert!(
            result
                .modules
                .iter()
                .all(|m| m.signature == ModuleSignature::NotChecked
                    && m.signer.is_none()
                    && m.process == ModuleProcess::Core)
        );
    }

    /// GIVEN a child process told to preload the system C library WHEN
    /// scanned THEN that library is reported as preloaded, although it is a
    /// system file; GIVEN the child has exited WHEN scanned THEN it is not
    /// counted as incomplete.
    #[test]
    fn given_a_preloading_child_then_its_preload_is_reported_and_an_exit_is_not_incomplete() {
        let libc = mapped_libc();
        let mut child = Command::new("sleep")
            .arg("30")
            .env("LD_PRELOAD", &libc)
            .stdin(Stdio::null())
            .spawn()
            .expect("sleep starts");
        let pid = child.id();
        // Wait until the loader has mapped the libraries.
        let start = std::time::Instant::now();
        let result = loop {
            let result = scan(pid, ModuleProcess::WebView);
            if result.modules.iter().any(|m| m.path == libc)
                || start.elapsed() > std::time::Duration::from_secs(10)
            {
                break result;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let row = result.modules.iter().find(|m| m.path == libc);
        assert_eq!(row.map(|m| m.reason), Some(ModuleReason::Preloaded));
        assert!(!result.incomplete);
        child.kill().unwrap();
        child.wait().unwrap();
        let after = scan(pid, ModuleProcess::WebView);
        assert!(after.modules.is_empty());
        assert!(!after.incomplete, "an exited process is not incomplete");
    }

    /// GIVEN executable memory with no file in a child that runs this test
    /// binary WHEN scanned as the core THEN it is reported with no file; as
    /// a web view process THEN it is not, because a JIT makes it normal there.
    #[test]
    fn given_fileless_executable_memory_then_only_the_core_reports_it() {
        let exe = fs::read_link("/proc/self/exe").unwrap();
        let mut child = Command::new(exe)
            .args([
                "--exact",
                "platform::linux::modules::tests::fileless_child",
                "--ignored",
                "--nocapture",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the helper starts");
        // The helper signals on stderr: libtest writes to stdout.
        let mut ready = [0u8; 6];
        child
            .stderr
            .as_mut()
            .unwrap()
            .read_exact(&mut ready)
            .expect("the helper says ready");
        let pid = child.id();
        let as_core = scan(pid, ModuleProcess::Core);
        let as_web = scan(pid, ModuleProcess::WebView);
        drop(child.stdin.take());
        child.wait().unwrap();
        assert!(
            as_core
                .modules
                .iter()
                .any(|m| m.reason == ModuleReason::NoFile && m.path == Path::new(ANONYMOUS)),
            "anonymous executable memory in the core is reported"
        );
        assert!(
            !as_web
                .modules
                .iter()
                .any(|m| m.reason == ModuleReason::NoFile),
            "not in a web view process"
        );
    }

    /// Helper for the test above: maps anonymous executable memory, says
    /// "ready", and waits for its input to close.
    #[test]
    #[ignore = "runs in a child process of given_fileless_executable_memory_then_only_the_core_reports_it"]
    fn fileless_child() {
        // SAFETY: an anonymous private mapping; nothing is run from it.
        let page = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(page, libc::MAP_FAILED);
        eprintln!("ready");
        let mut rest = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut rest);
    }
}
