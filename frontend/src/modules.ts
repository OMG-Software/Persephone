// Text for the report of code loaded into Persephone. Pure functions, no host
// calls. Every file name, path and signer is the machine's word: the page
// shows it as isolated, escaped text.

import type {
  ModuleProcess,
  ModuleReason,
  ModuleScanState,
  ThirdPartyModule,
  ThirdPartyModules
} from '../../crates/contracts/generated/contracts';

/** The core's limits on one acknowledgement. */
export const MAX_ACKNOWLEDGED_MODULES = 128;
export const MAX_MODULE_ID_BYTES = 512;

/** Where a module is loaded, in the user's words. */
export function processLabel(process: ModuleProcess): string {
  return process === 'core' ? 'Persephone core' : process === 'ui' ? 'Window' : 'Web view';
}

/** The processes of a module, such as "Persephone core, Web view". */
export function processesLabel(processes: readonly ModuleProcess[]): string {
  return processes.map(processLabel).join(', ');
}

/** Who signed a module: the signer, or why there is none. */
export function signerLabel(module: Pick<ThirdPartyModule, 'signature' | 'signer'>): string {
  if (module.signature === 'unsigned') return 'Unsigned';
  if (module.signature === 'invalid') return 'Signature not valid';
  if (module.signature === 'not_checked') return 'Not checked';
  return module.signer ?? 'Signed';
}

/** Why Linux lists a module, in the user's words. */
const REASON_TEXT: Record<Exclude<ModuleReason, 'signer'>, string> = {
  preloaded: 'Forced in by LD_PRELOAD',
  no_file: 'No file on disk',
  user_writable: 'Can be changed without admin rights',
  outside_system: 'Not a system file'
};

/** Why a module is listed: its signer, or the Linux reason. */
export function reasonLabel(
  module: Pick<ThirdPartyModule, 'signature' | 'signer' | 'reason'>
): string {
  return module.reason === 'signer' ? signerLabel(module) : REASON_TEXT[module.reason];
}

/** True when the rows are listed for a reason other than their signer (Linux). */
export function listsReasons(modules: readonly Pick<ThirdPartyModule, 'reason'>[]): boolean {
  return modules.some((module) => module.reason !== 'signer');
}

/** The desktop system, for text that differs between them. */
export type DesktopPlatform = 'windows' | 'linux' | 'mac' | 'other';

/** The desktop system from `navigator.platform`. */
export function platformOf(platform: string): DesktopPlatform {
  if (/^Win/.test(platform)) return 'windows';
  if (/Mac/.test(platform)) return 'mac';
  if (/Linux/.test(platform)) return 'linux';
  return 'other';
}

/** What the list shows on each system. */
export const LEDE_TEXT: Record<DesktopPlatform, string> = {
  windows:
    'Windows and security software can load their own code into apps. Persephone blocks ' +
    'hook-based code and code that Microsoft did not sign. This list shows the other code that ' +
    'is loaded.',
  linux:
    'Other programs can force code into apps on Linux, for example through LD_PRELOAD. This ' +
    'list shows code in Persephone that is not a protected system file or part of Persephone.',
  mac:
    'The signed release of Persephone loads only code from Apple and from Persephone. This ' +
    'list shows other code loaded into the Persephone core. macOS does not let Persephone ' +
    'check its window and web view.',
  other: 'This list shows code loaded into Persephone that Persephone did not ship.'
};

/** Shown when the last scan could not read every process or module. */
export const INCOMPLETE_TEXT =
  'Some Persephone processes could not be checked. Code loaded into them is not listed.';

/** The text of a scan state that has no list to show, or null when there is one. */
export function stateText(
  report: Pick<ThirdPartyModules, 'state' | 'modules' | 'incomplete'>
): string | null {
  const state: ModuleScanState = report.state;
  if (state === 'pending') return 'Checking…';
  if (state === 'unsupported') return 'Not available on this system.';
  if (report.modules.length > 0) return null;
  // An empty list from a partial scan is not a clean result.
  return report.incomplete ? INCOMPLETE_TEXT : 'No other code is loaded.';
}

/** The modules the user has not reviewed, in the report's order. */
export function unreviewed(report: ThirdPartyModules | null): ThirdPartyModule[] {
  return report ? report.modules.filter((module) => !module.acknowledged) : [];
}

/** The ids to acknowledge for these modules, within the core's limits. */
export function acknowledgeIds(modules: readonly ThirdPartyModule[]): string[] {
  const encoder = new TextEncoder();
  return modules
    .map((module) => module.id)
    .filter((id) => id.length > 0 && encoder.encode(id).length <= MAX_MODULE_ID_BYTES)
    .slice(0, MAX_ACKNOWLEDGED_MODULES);
}

/** The one-line notice for unreviewed modules, or null when there are none. */
export function noticeText(report: ThirdPartyModules | null): string | null {
  const modules = unreviewed(report);
  if (modules.length === 0) return null;
  const first = modules[0];
  const more = modules.length - 1;
  const base = `Other code is loaded into Persephone: ${first.fileName} (${reasonLabel(first)})`;
  return more > 0 ? `${base} and ${more} more` : base;
}
