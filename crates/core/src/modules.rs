//! The report of third-party code loaded into Persephone.
//!
//! * A scan thread lists the modules (DLLs and EXEs) in the core, the UI and
//!   its web view processes that are not expected ones
//!   (`platform::windows::modules`). This file keeps the platform-free part:
//!   it merges the scans, applies the user's reviews and projects the
//!   result for the UI. Nothing here is printed or logged: paths can contain
//!   the user name.
//! * A module's review key (its `id`) is its lowercase file name and its
//!   signer, or `unsigned` or `invalid`. A reviewed file therefore stays
//!   reviewed after an update, and a changed signer is new again.
//! * `first_seen` is kept for the life of the core process.
//! * The reviewed ids are kept in the encrypted crypto store of the open
//!   profile as one JSON list, at most [`MAX_REVIEWED`] (the oldest dropped
//!   first), so each profile has its own and signing out wipes it. While a
//!   store is open, the ids in memory are the store's. A lock or sign-out
//!   clears them. With no store open (demo, signed out, locked) they live
//!   only in memory. A record that cannot be read means none were reviewed:
//!   showing more is the safe side.
//! * A scan that could not read every target process, or every module of
//!   one, is reported as incomplete, so an empty list is not read as clean.
//! * File names, signers and paths are the files' word, projected like
//!   names. A report never lists more than [`MAX_THIRD_PARTY_MODULES`]
//!   modules or [`MAX_REPORT_BYTES`] bytes, and says when it is cut.

use crate::store::Stores;
use crate::views::{is_bidi_control, project_name};
use matrix_sdk_crypto::store::CryptoStore;
use persephone_contracts::{
    ModuleProcess, ModuleReason, ModuleScanState, ModuleSignature, ThirdPartyModule,
    ThirdPartyModules, UnixMillis,
};
use persephone_core_protocol::{
    ErrorCode, MAX_MODULE_ID_BYTES, MAX_MODULE_PATH_BYTES, MAX_THIRD_PARTY_MODULES,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, btree_map::Entry as Entry2},
    path::{Path, PathBuf},
};

pub const REVIEWED_MODULES_KEY: &str = "persephone.reviewed_modules.v1";
/// Most reviewed ids kept.
pub const MAX_REVIEWED: usize = 200;
const MAX_REVIEWED_BYTES: usize = 64 * 1024;
/// Most distinct files one report is built from, and most `first_seen` times
/// kept. A process cannot grow them without limit.
const MAX_TRACKED: usize = 4096;
/// Serialized size of the modules in one reply, under the 128 KiB control
/// frame with room for the envelope.
pub const MAX_REPORT_BYTES: usize = 96 * 1024;
/// The id parts: the separator, and most bytes of the signature part.
const ID_SEPARATOR: char = '\u{1f}';
const MAX_ID_SUFFIX_BYTES: usize = 256;

/// One module as the scan found it. Raw: nothing is projected yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub process: ModuleProcess,
    pub signature: ModuleSignature,
    /// The signer's simple name. Only used for a valid signature.
    pub signer: Option<String>,
    /// Why it is listed. `Signer` where signatures are checked.
    pub reason: ModuleReason,
}

#[cfg(windows)]
impl From<crate::platform::windows::modules::FoundModule> for Found {
    fn from(found: crate::platform::windows::modules::FoundModule) -> Self {
        Self {
            path: found.path,
            process: found.process,
            signature: found.signature,
            signer: found.signer,
            reason: ModuleReason::Signer,
        }
    }
}

/// How serious a reason is, for a file listed for different reasons in
/// different processes: the most serious one is shown.
fn severity(reason: ModuleReason) -> u8 {
    match reason {
        ModuleReason::Signer => 0,
        ModuleReason::OutsideSystem => 1,
        ModuleReason::UserWritable => 2,
        ModuleReason::NoFile => 3,
        ModuleReason::Preloaded => 4,
    }
}

/// One file, projected, in every process it is loaded in.
struct Entry {
    module: ThirdPartyModule,
    /// Lowercase file name, then path, for the order.
    sort_key: (String, String),
}

struct Scan {
    at: UnixMillis,
    entries: Vec<Entry>,
    incomplete: bool,
}

/// The merged scans and the reviews.
pub struct Modules {
    supported: bool,
    scan: Option<Scan>,
    first_seen: HashMap<String, UnixMillis>,
    /// Reviewed ids, oldest first.
    reviewed: Vec<String>,
}

impl Default for Modules {
    /// Supported where the core can list modules: Windows, Linux and macOS.
    fn default() -> Self {
        Self::with_support(cfg!(any(windows, target_os = "linux", target_os = "macos")))
    }
}

impl Modules {
    /// A state for a platform that can list modules, or cannot.
    pub fn with_support(supported: bool) -> Self {
        Self {
            supported,
            scan: None,
            first_seen: HashMap::new(),
            reviewed: Vec::new(),
        }
    }

    /// Takes a finished scan; `incomplete` when a target process or some of
    /// its modules could not be read. Returns whether the report changed: a
    /// rescan with the same result is not a change.
    pub fn update(&mut self, found: Vec<Found>, incomplete: bool, now: UnixMillis) -> bool {
        let before = self.report();
        let entries = merge(found);
        for entry in &entries {
            if self.first_seen.len() < MAX_TRACKED {
                self.first_seen
                    .entry(entry.module.id.clone())
                    .or_insert(now);
            }
        }
        let entries = entries
            .into_iter()
            .map(|mut entry| {
                entry.module.first_seen = self
                    .first_seen
                    .get(&entry.module.id)
                    .copied()
                    .unwrap_or(now);
                entry
            })
            .collect();
        self.scan = Some(Scan {
            at: now,
            entries,
            incomplete,
        });
        changed(&before, &self.report())
    }

    /// Marks the ids that are in the current report as reviewed; others are
    /// ignored, so the page cannot fill the list with anything else.
    /// Returns whether the report changed.
    pub fn acknowledge(&mut self, ids: &[String]) -> bool {
        let present: BTreeSet<&str> = self.entries().map(|m| m.id.as_str()).collect();
        let wanted: Vec<String> = ids
            .iter()
            .filter(|id| present.contains(id.as_str()))
            .cloned()
            .collect();
        self.add_reviewed(wanted)
    }

    /// Makes the reviewed ids those the open profile's store holds, so no id
    /// from another profile or from before sign-in stays. Returns whether the
    /// report changed.
    pub fn replace_reviewed(&mut self, stored: &[String]) -> bool {
        let before = self.report();
        self.reviewed.clear();
        self.add_reviewed(stored.to_vec());
        changed(&before, &self.report())
    }

    /// Forgets every reviewed id, when a lock or sign-out closes the store.
    pub fn clear_reviewed(&mut self) {
        self.reviewed.clear();
    }

    /// The reviewed ids, oldest first, at most [`MAX_REVIEWED`].
    pub fn reviewed(&self) -> &[String] {
        &self.reviewed
    }

    fn add_reviewed(&mut self, ids: Vec<String>) -> bool {
        let shown: BTreeSet<String> = self
            .entries()
            .filter(|m| !self.reviewed.contains(&m.id))
            .map(|m| m.id.clone())
            .collect();
        let mut changed = false;
        for id in ids {
            if id.is_empty() || id.len() > MAX_MODULE_ID_BYTES || self.reviewed.contains(&id) {
                continue;
            }
            changed |= shown.contains(&id);
            self.reviewed.push(id);
        }
        let excess = self.reviewed.len().saturating_sub(MAX_REVIEWED);
        self.reviewed.drain(..excess);
        changed
    }

    fn entries(&self) -> impl Iterator<Item = &ThirdPartyModule> {
        self.scan
            .iter()
            .flat_map(|scan| scan.entries.iter().map(|entry| &entry.module))
    }

    /// The report for the UI.
    pub fn report(&self) -> ThirdPartyModules {
        let state = match (&self.scan, self.supported) {
            (_, false) => ModuleScanState::Unsupported,
            (None, true) => ModuleScanState::Pending,
            (Some(_), true) => ModuleScanState::Ready,
        };
        let Some(scan) = self.scan.as_ref().filter(|_| self.supported) else {
            return ThirdPartyModules {
                state,
                scanned_at: None,
                modules: Vec::new(),
                truncated: false,
                incomplete: false,
            };
        };
        let mut all: Vec<(&Entry, bool)> = scan
            .entries
            .iter()
            .map(|entry| (entry, self.reviewed.contains(&entry.module.id)))
            .collect();
        // Unreviewed first; the entries are already by name and path.
        all.sort_by_key(|(_, reviewed)| *reviewed);
        let mut truncated = all.len() > MAX_THIRD_PARTY_MODULES;
        let mut bytes = 0;
        let mut modules = Vec::new();
        for (entry, reviewed) in all.into_iter().take(MAX_THIRD_PARTY_MODULES) {
            let mut module = entry.module.clone();
            module.acknowledged = reviewed;
            bytes += serde_json::to_vec(&module).map_or(usize::MAX, |json| json.len() + 1);
            if bytes > MAX_REPORT_BYTES {
                truncated = true;
                break;
            }
            modules.push(module);
        }
        ThirdPartyModules {
            state,
            scanned_at: Some(scan.at),
            modules,
            truncated,
            incomplete: scan.incomplete,
        }
    }
}

/// Whether the UI would show something different. The scan time is not.
fn changed(before: &ThirdPartyModules, after: &ThirdPartyModules) -> bool {
    let mut before = before.clone();
    before.scanned_at = after.scanned_at;
    before != *after
}

/// Merges the same file in several processes, and projects it. The result is
/// by lowercase file name, then path, at most [`MAX_TRACKED`] files.
fn merge(found: Vec<Found>) -> Vec<Entry> {
    let mut files: BTreeMap<String, (Found, Vec<ModuleProcess>)> = BTreeMap::new();
    for item in found {
        let key = path_key(&item.path);
        let tracked = files.len();
        match files.entry(key) {
            Entry2::Occupied(mut known) => {
                let (first, processes) = known.get_mut();
                processes.push(item.process);
                if severity(item.reason) > severity(first.reason) {
                    first.reason = item.reason;
                }
            }
            Entry2::Vacant(slot) if tracked < MAX_TRACKED => {
                slot.insert((item.clone(), vec![item.process]));
            }
            Entry2::Vacant(_) => {}
        }
    }
    let mut entries: Vec<Entry> = files
        .into_values()
        .map(|(item, mut processes)| {
            processes.sort();
            processes.dedup();
            project(&item, processes)
        })
        .collect();
    entries.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));
    entries
}

fn project(item: &Found, processes: Vec<ModuleProcess>) -> Entry {
    // The paths are Windows paths, also in tests on other hosts, so both
    // separators end the name, not only the host's.
    let path_text = item.path.to_string_lossy();
    let raw_name = path_text.rsplit(['\\', '/']).next().unwrap_or_default();
    let file_name = match project_name(raw_name) {
        name if name.is_empty() => "?".to_owned(),
        name => name,
    };
    let signer = (item.signature == ModuleSignature::Valid)
        .then(|| item.signer.as_deref().map(project_name))
        .flatten()
        .filter(|signer| !signer.is_empty());
    let path = display_path(&item.path);
    let id = module_id(&file_name, item.signature, signer.as_deref(), item.reason);
    Entry {
        sort_key: (file_name.to_lowercase(), path.clone()),
        module: ThirdPartyModule {
            id,
            file_name,
            path,
            signer,
            signature: item.signature,
            reason: item.reason,
            processes,
            first_seen: UnixMillis::saturating(0),
            acknowledged: false,
        },
    }
}

/// The review key: lowercase file name, a separator and the signer, or
/// `unsigned` or `invalid`. Where no signature is checked (Linux), the
/// reason takes the signer's place, so a file listed for a new reason is new
/// again. The name is cut, not the suffix, when the key is too long.
fn module_id(
    file_name: &str,
    signature: ModuleSignature,
    signer: Option<&str>,
    reason: ModuleReason,
) -> String {
    let suffix = match (signature, signer) {
        (ModuleSignature::Valid, Some(signer)) => signer,
        (ModuleSignature::Valid, None) => "valid",
        (ModuleSignature::Unsigned, _) => "unsigned",
        (ModuleSignature::Invalid, _) => "invalid",
        (ModuleSignature::NotChecked, _) => match reason {
            ModuleReason::Signer => "not_checked",
            ModuleReason::Preloaded => "preloaded",
            ModuleReason::UserWritable => "user_writable",
            ModuleReason::OutsideSystem => "outside_system",
            ModuleReason::NoFile => "no_file",
        },
    };
    let suffix = cut_bytes(suffix, MAX_ID_SUFFIX_BYTES);
    let lower = file_name.to_lowercase();
    let name = cut_bytes(
        &lower,
        MAX_MODULE_ID_BYTES - ID_SEPARATOR.len_utf8() - suffix.len(),
    );
    format!("{name}{ID_SEPARATOR}{suffix}")
}

/// The longest prefix of `text` of at most `max` bytes, cut on a character
/// boundary.
fn cut_bytes(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The path without the `\\?\` prefix, controls replaced, cut to
/// [`MAX_MODULE_PATH_BYTES`] bytes with "…" inside the limit.
fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    let clean: String = text
        .chars()
        .map(|c| {
            if c.is_control() || is_bidi_control(c) || c == '\u{FEFF}' {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect();
    if clean.len() <= MAX_MODULE_PATH_BYTES {
        return clean;
    }
    let mut cut = cut_bytes(&clean, MAX_MODULE_PATH_BYTES - '…'.len_utf8()).to_owned();
    cut.push('…');
    cut
}

/// The key for the same file in two processes: no `\\?\` prefix, `\`
/// separators, lowercase.
fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    text.replace('/', r"\").to_lowercase()
}

/// The reviewed ids a store holds, oldest first. A record that cannot be
/// read is an error, which the caller treats as none reviewed.
pub async fn load(stores: &Stores) -> Result<Vec<String>, ErrorCode> {
    let Some(bytes) = stores
        .crypto()
        .get_custom_value(REVIEWED_MODULES_KEY)
        .await
        .map_err(|_| ErrorCode::Internal)?
    else {
        return Ok(Vec::new());
    };
    decode(&bytes)
}

/// Stores the reviewed ids, at most [`MAX_REVIEWED`] (the oldest dropped).
pub async fn save(stores: &Stores, reviewed: &[String]) -> Result<(), ErrorCode> {
    let bytes = encode(reviewed)?;
    stores
        .crypto()
        .set_custom_value(REVIEWED_MODULES_KEY, bytes)
        .await
        .map_err(|_| ErrorCode::Internal)
}

fn decode(bytes: &[u8]) -> Result<Vec<String>, ErrorCode> {
    if bytes.len() > MAX_REVIEWED_BYTES {
        return Err(ErrorCode::Internal);
    }
    serde_json::from_slice(bytes).map_err(|_| ErrorCode::Internal)
}

fn encode(reviewed: &[String]) -> Result<Vec<u8>, ErrorCode> {
    let excess = reviewed.len().saturating_sub(MAX_REVIEWED);
    let bytes = serde_json::to_vec(&reviewed[excess..]).map_err(|_| ErrorCode::Internal)?;
    if bytes.len() > MAX_REVIEWED_BYTES {
        return Err(ErrorCode::Internal);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: u64) -> UnixMillis {
        UnixMillis::saturating(millis)
    }

    fn found(path: &str, process: ModuleProcess, signer: Option<&str>) -> Found {
        Found {
            path: PathBuf::from(path),
            process,
            signature: if signer.is_some() {
                ModuleSignature::Valid
            } else {
                ModuleSignature::Unsigned
            },
            signer: signer.map(ToOwned::to_owned),
            reason: ModuleReason::Signer,
        }
    }

    fn linux(path: &str, process: ModuleProcess, reason: ModuleReason) -> Found {
        Found {
            path: PathBuf::from(path),
            process,
            signature: ModuleSignature::NotChecked,
            signer: None,
            reason,
        }
    }

    /// GIVEN Linux rows, which have no signature to check, WHEN listed THEN
    /// the reason takes the signer's place in the id, the row has no signer,
    /// and a file listed for different reasons in different processes shows
    /// the most serious one.
    #[test]
    fn given_linux_rows_then_the_reason_keys_the_review_and_the_worst_reason_shows() {
        let modules = ready(vec![
            linux(
                "/home/a/.local/lib/hook.so",
                ModuleProcess::Ui,
                ModuleReason::UserWritable,
            ),
            linux(
                "/home/a/.local/lib/hook.so",
                ModuleProcess::Core,
                ModuleReason::Preloaded,
            ),
            linux(
                "/opt/vendor/lib/plugin.so",
                ModuleProcess::WebView,
                ModuleReason::OutsideSystem,
            ),
            linux("[anonymous]", ModuleProcess::Core, ModuleReason::NoFile),
        ]);
        let report = modules.report();
        let rows: Vec<(&str, String, ModuleReason, Vec<ModuleProcess>)> = report
            .modules
            .iter()
            .map(|m| {
                (
                    m.file_name.as_str(),
                    m.id.clone(),
                    m.reason,
                    m.processes.clone(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    "[anonymous]",
                    id("[anonymous]", "no_file"),
                    ModuleReason::NoFile,
                    vec![ModuleProcess::Core]
                ),
                (
                    "hook.so",
                    id("hook.so", "preloaded"),
                    ModuleReason::Preloaded,
                    vec![ModuleProcess::Core, ModuleProcess::Ui]
                ),
                (
                    "plugin.so",
                    id("plugin.so", "outside_system"),
                    ModuleReason::OutsideSystem,
                    vec![ModuleProcess::WebView]
                ),
            ]
        );
        assert!(
            report
                .modules
                .iter()
                .all(|m| m.signer.is_none() && m.signature == ModuleSignature::NotChecked)
        );
        // A reviewed file listed later for another reason is new again.
        let mut later = ready(vec![linux(
            "/opt/vendor/lib/plugin.so",
            ModuleProcess::Ui,
            ModuleReason::OutsideSystem,
        )]);
        later.acknowledge(&[id("plugin.so", "outside_system")]);
        assert!(later.report().modules[0].acknowledged);
        later.update(
            vec![linux(
                "/opt/vendor/lib/plugin.so",
                ModuleProcess::Ui,
                ModuleReason::UserWritable,
            )],
            false,
            at(2_000),
        );
        assert!(!later.report().modules[0].acknowledged);
    }

    fn ready(items: Vec<Found>) -> Modules {
        let mut modules = Modules::with_support(true);
        modules.update(items, false, at(1_000));
        modules
    }

    fn id(name: &str, suffix: &str) -> String {
        format!("{name}\u{1f}{suffix}")
    }

    /// GIVEN valid, unsigned and invalid files WHEN listed THEN each id is the
    /// lowercase name and the signer, `unsigned` or `invalid`, and only a
    /// valid file has a signer.
    #[test]
    fn given_each_signature_then_the_id_and_signer_follow_the_rule() {
        let mut bad = found(r"C:\x\Bad.dll", ModuleProcess::Core, None);
        bad.signature = ModuleSignature::Invalid;
        bad.signer = Some("Not shown".into());
        let modules = ready(vec![
            found(r"C:\x\Hook.DLL", ModuleProcess::Core, Some("Sophos Ltd")),
            found(r"C:\x\Plain.dll", ModuleProcess::Core, None),
            bad,
        ]);
        let report = modules.report();
        assert_eq!(report.state, ModuleScanState::Ready);
        let by_name = |name: &str| {
            report
                .modules
                .iter()
                .find(|m| m.file_name == name)
                .unwrap()
                .clone()
        };
        assert_eq!(by_name("Hook.DLL").id, id("hook.dll", "Sophos Ltd"));
        assert_eq!(by_name("Hook.DLL").signer.as_deref(), Some("Sophos Ltd"));
        assert_eq!(by_name("Plain.dll").id, id("plain.dll", "unsigned"));
        assert_eq!(by_name("Plain.dll").signer, None);
        assert_eq!(by_name("Bad.dll").id, id("bad.dll", "invalid"));
        assert_eq!(by_name("Bad.dll").signer, None);
    }

    /// GIVEN the same file in three processes, with another spelling of its
    /// path WHEN merged THEN it is one row with the processes sorted and
    /// unique, and a file of the same name elsewhere is its own row.
    #[test]
    fn given_one_file_in_many_processes_then_it_is_one_row() {
        let modules = ready(vec![
            found(r"C:\x\a.dll", ModuleProcess::WebView, None),
            found(r"\\?\C:\X\A.DLL", ModuleProcess::Core, None),
            found("C:/x/a.dll", ModuleProcess::WebView, None),
            found(r"C:\y\a.dll", ModuleProcess::Ui, None),
        ]);
        let report = modules.report();
        assert_eq!(report.modules.len(), 2);
        let first = &report.modules[0];
        assert_eq!(first.path, r"C:\x\a.dll");
        assert_eq!(
            first.processes,
            [ModuleProcess::Core, ModuleProcess::WebView]
        );
        assert_eq!(report.modules[1].processes, [ModuleProcess::Ui]);
    }

    /// GIVEN reviewed and new files WHEN listed THEN the new come first, then
    /// by lowercase name, then by path.
    #[test]
    fn given_reviewed_files_then_unreviewed_ones_come_first() {
        let mut modules = ready(vec![
            found(r"C:\b\B.dll", ModuleProcess::Core, None),
            found(r"C:\z\a.dll", ModuleProcess::Core, None),
            found(r"C:\a\a.dll", ModuleProcess::Core, None),
            found(r"C:\c\C.dll", ModuleProcess::Core, None),
        ]);
        assert!(modules.acknowledge(&[id("a.dll", "unsigned")]));
        let report = modules.report();
        let order: Vec<(&str, bool)> = report
            .modules
            .iter()
            .map(|m| (m.path.as_str(), m.acknowledged))
            .collect();
        assert_eq!(
            order,
            [
                (r"C:\b\B.dll", false),
                (r"C:\c\C.dll", false),
                (r"C:\a\a.dll", true),
                (r"C:\z\a.dll", true),
            ]
        );
    }

    /// GIVEN more files than the limit WHEN listed THEN the report holds the
    /// limit and says it is cut; GIVEN exactly the limit THEN it does not.
    #[test]
    fn given_too_many_files_then_the_report_is_cut_and_says_so() {
        let many = |count: usize| {
            (0..count)
                .map(|n| found(&format!(r"C:\x\m{n:04}.dll"), ModuleProcess::Core, None))
                .collect()
        };
        let report = ready(many(MAX_THIRD_PARTY_MODULES + 1)).report();
        assert_eq!(report.modules.len(), MAX_THIRD_PARTY_MODULES);
        assert!(report.truncated);
        let report = ready(many(MAX_THIRD_PARTY_MODULES)).report();
        assert_eq!(report.modules.len(), MAX_THIRD_PARTY_MODULES);
        assert!(!report.truncated);
    }

    /// GIVEN rows that together are larger than the reply budget WHEN listed
    /// THEN the report stays within it and says it is cut.
    #[test]
    fn given_large_rows_then_the_report_stays_in_the_byte_budget() {
        let long = "d".repeat(900);
        let items = (0..MAX_THIRD_PARTY_MODULES)
            .map(|n| found(&format!(r"C:\{long}\{n:03}.dll"), ModuleProcess::Core, None))
            .collect();
        let report = ready(items).report();
        let bytes = serde_json::to_vec(&report.modules).unwrap().len();
        assert!(bytes <= MAX_REPORT_BYTES);
        assert!(report.modules.len() < MAX_THIRD_PARTY_MODULES);
        assert!(report.truncated);
    }

    /// GIVEN a hostile file name, signer and path WHEN listed THEN controls
    /// and bidi marks are replaced, the name and signer are at most 128
    /// characters, the path and id are within their byte limits on character
    /// boundaries, and the id still ends with the signature part.
    #[test]
    fn given_hostile_text_then_it_is_projected_and_bounded() {
        let name = format!("ev\u{202E}il\u{7}{}.dll", "é".repeat(400));
        let path = format!("C:\\{}\\{name}", "ü".repeat(900));
        let signer = format!("Evil\nCorp{}", "ß".repeat(400));
        let modules = ready(vec![found(&path, ModuleProcess::Ui, Some(&signer))]);
        let module = modules.report().modules.remove(0);
        assert!(module.file_name.starts_with("ev\u{FFFD}il\u{FFFD}"));
        assert!(module.file_name.chars().count() <= 129);
        assert!(module.file_name.ends_with('…'));
        let signer = module.signer.expect("a valid signer");
        assert!(signer.starts_with("Evil Corp"));
        assert!(signer.chars().count() <= 129);
        assert!(module.path.len() <= MAX_MODULE_PATH_BYTES);
        assert!(module.path.ends_with('…'));
        assert!(module.id.len() <= MAX_MODULE_ID_BYTES);
        assert!(module.id.contains('\u{1f}'));
        assert!(!module.id.contains('\u{7}'));
        assert!(module.id.ends_with(cut_bytes(&signer, MAX_ID_SUFFIX_BYTES)));
    }

    /// GIVEN a path with a prefix and controls WHEN shown THEN the prefix is
    /// gone and a control is replaced.
    #[test]
    fn given_a_prefixed_path_then_it_is_shown_without_the_prefix() {
        let modules = ready(vec![found(
            "\\\\?\\C:\\a\nb\\x.dll",
            ModuleProcess::Core,
            None,
        )]);
        assert_eq!(modules.report().modules[0].path, "C:\\a\u{FFFD}b\\x.dll");
    }

    /// GIVEN ids that are not in the report WHEN acknowledged THEN they are
    /// ignored and nothing is reviewed; known ids are reviewed.
    #[test]
    fn given_an_unknown_id_then_it_is_ignored() {
        let mut modules = ready(vec![found(r"C:\x\a.dll", ModuleProcess::Core, None)]);
        assert!(!modules.acknowledge(&["nothing".into(), String::new()]));
        assert!(modules.reviewed().is_empty());
        assert!(modules.acknowledge(&["nothing".into(), id("a.dll", "unsigned")]));
        assert_eq!(modules.reviewed(), [id("a.dll", "unsigned")]);
        assert!(modules.report().modules[0].acknowledged);
        assert!(!modules.acknowledge(&[id("a.dll", "unsigned")]));
    }

    /// GIVEN ids read from the store WHEN applied THEN a listed file is
    /// reviewed and an unlisted id is kept for later; the same ids again are
    /// no change.
    #[test]
    fn given_stored_ids_then_they_are_applied() {
        let mut modules = ready(vec![found(r"C:\x\a.dll", ModuleProcess::Core, None)]);
        let stored = vec![id("old.dll", "unsigned"), id("a.dll", "unsigned")];
        assert!(modules.replace_reviewed(&stored));
        assert!(modules.report().modules[0].acknowledged);
        assert_eq!(modules.reviewed(), stored);
        assert!(!modules.replace_reviewed(&stored));
        // A file that appears later is already reviewed.
        let changed = modules.update(
            vec![found(r"C:\x\old.dll", ModuleProcess::Core, None)],
            false,
            at(2_000),
        );
        assert!(changed);
        assert!(modules.report().modules[0].acknowledged);
    }

    /// GIVEN more reviewed ids than the limit WHEN added THEN the oldest are
    /// dropped.
    #[test]
    fn given_too_many_reviewed_ids_then_the_oldest_are_dropped() {
        let mut modules = Modules::with_support(true);
        let ids: Vec<String> = (0..MAX_REVIEWED + 5).map(|n| format!("n{n}")).collect();
        modules.replace_reviewed(&ids);
        assert_eq!(modules.reviewed().len(), MAX_REVIEWED);
        assert_eq!(modules.reviewed()[0], "n5");
    }

    /// GIVEN a scan WHEN the same result is scanned again THEN there is no
    /// change and `first_seen` stays; WHEN a process or a file is added or
    /// goes THEN there is a change; WHEN a file returns THEN it keeps its
    /// first time.
    #[test]
    fn given_a_rescan_then_only_a_different_result_is_a_change() {
        let mut modules = Modules::with_support(true);
        let one = || vec![found(r"C:\x\a.dll", ModuleProcess::Core, None)];
        assert!(modules.update(one(), false, at(1_000)));
        assert!(!modules.update(one(), false, at(2_000)));
        assert_eq!(modules.report().modules[0].first_seen, at(1_000));
        assert_eq!(modules.report().scanned_at, Some(at(2_000)));
        let two = vec![
            found(r"C:\x\a.dll", ModuleProcess::Core, None),
            found(r"C:\x\a.dll", ModuleProcess::Ui, None),
        ];
        assert!(modules.update(two, false, at(3_000)));
        assert!(modules.update(Vec::new(), false, at(4_000)));
        assert!(modules.update(one(), false, at(5_000)));
        assert_eq!(modules.report().modules[0].first_seen, at(1_000));
    }

    /// GIVEN no scan yet THEN the state is pending, and an empty first scan
    /// is a change to ready; GIVEN a platform that cannot list THEN the state
    /// is unsupported and a scan shows nothing.
    #[test]
    fn given_the_platform_and_scan_then_the_state_follows() {
        let mut modules = Modules::with_support(true);
        let pending = modules.report();
        assert_eq!(pending.state, ModuleScanState::Pending);
        assert_eq!(pending.scanned_at, None);
        assert!(modules.update(Vec::new(), false, at(1)));
        assert_eq!(modules.report().state, ModuleScanState::Ready);

        let mut other = Modules::with_support(false);
        assert_eq!(other.report().state, ModuleScanState::Unsupported);
        other.update(
            vec![found(r"C:\x\a.dll", ModuleProcess::Core, None)],
            false,
            at(1),
        );
        let report = other.report();
        assert_eq!(report.state, ModuleScanState::Unsupported);
        assert!(report.modules.is_empty());
    }

    /// GIVEN ids reviewed in memory, as before sign-in or by another profile
    /// WHEN the open profile's stored ids replace them THEN only the stored
    /// ids are reviewed; WHEN a lock clears them THEN nothing is reviewed.
    #[test]
    fn given_other_reviews_then_the_stored_ids_replace_them() {
        let mut modules = ready(vec![
            found(r"C:\x\a.dll", ModuleProcess::Core, None),
            found(r"C:\x\b.dll", ModuleProcess::Core, None),
        ]);
        assert!(modules.acknowledge(&[id("a.dll", "unsigned")]));
        assert!(modules.replace_reviewed(&[id("b.dll", "unsigned")]));
        assert_eq!(modules.reviewed(), [id("b.dll", "unsigned")]);
        let report = modules.report();
        let flags: Vec<(&str, bool)> = report
            .modules
            .iter()
            .map(|m| (m.file_name.as_str(), m.acknowledged))
            .collect();
        assert_eq!(flags, [("a.dll", false), ("b.dll", true)]);
        modules.clear_reviewed();
        assert!(modules.reviewed().is_empty());
        assert!(modules.report().modules.iter().all(|m| !m.acknowledged));
    }

    /// GIVEN a scan that could not read every process WHEN reported THEN the
    /// report says it is incomplete, also when it lists nothing; the next
    /// complete scan clears that and is a change.
    #[test]
    fn given_an_incomplete_scan_then_the_report_says_so() {
        let mut modules = Modules::with_support(true);
        assert!(!modules.report().incomplete);
        assert!(modules.update(Vec::new(), true, at(1)));
        let report = modules.report();
        assert_eq!(report.state, ModuleScanState::Ready);
        assert!(report.modules.is_empty());
        assert!(report.incomplete);
        assert!(modules.update(Vec::new(), false, at(2)));
        assert!(!modules.report().incomplete);
    }

    /// GIVEN Windows paths with either separator WHEN listed on any host
    /// THEN the file name is the last part.
    #[test]
    fn given_either_separator_then_the_file_name_is_the_last_part() {
        let modules = ready(vec![
            found(r"C:\x\One.dll", ModuleProcess::Core, None),
            found("C:/y/Two.dll", ModuleProcess::Core, None),
        ]);
        let names: Vec<String> = modules
            .report()
            .modules
            .into_iter()
            .map(|m| m.file_name)
            .collect();
        assert_eq!(names, ["One.dll", "Two.dll"]);
    }

    /// GIVEN damaged, oversized or wrongly typed records WHEN decoded THEN
    /// each is an error, which the caller reads as none reviewed; a good
    /// record round-trips and an oversized list is cut to the newest ids.
    #[test]
    fn given_stored_records_then_only_a_good_one_is_read() {
        assert_eq!(decode(br#"["A","B"]"#), Ok(vec!["A".into(), "B".into()]));
        for bad in [&b"not json"[..], br#"{"A":1}"#, br#"[1]"#] {
            assert_eq!(decode(bad), Err(ErrorCode::Internal));
        }
        let mut large = b"[\"".to_vec();
        large.resize(MAX_REVIEWED_BYTES + 1, b'x');
        assert_eq!(decode(&large), Err(ErrorCode::Internal));

        let ids: Vec<String> = (0..MAX_REVIEWED + 3).map(|n| format!("n{n}")).collect();
        let saved = decode(&encode(&ids).unwrap()).unwrap();
        assert_eq!(saved.len(), MAX_REVIEWED);
        assert_eq!(saved[0], "n3");
        let huge = vec!["x".repeat(MAX_MODULE_ID_BYTES); MAX_REVIEWED];
        assert_eq!(encode(&huge), Err(ErrorCode::Internal));
    }
}
