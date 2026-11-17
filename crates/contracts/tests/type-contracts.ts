import type {
  AppStatus,
  CallDevices,
  CallEnd,
  CallJoinError,
  CallReadiness,
  CallStatus,
  CallUnavailable,
  LocalSearchFilters,
  ConnectionHealth,
  AttachmentKind,
  SyncMode,
  ImagePick,
  ImageView,
  ModuleProcess,
  ModuleScanState,
  ModuleSignature,
  PendingMessage,
  RoomChange,
  RoomPicture,
  RoomSettings,
  RoomSummary,
  ThirdPartyModule,
  ThirdPartyModules,
  TimelineItem,
} from "../generated/contracts.js";

const account = { state: "signed_out", userId: null, error: null } as const;

const invalidTag: TimelineItem = {
  // @ts-expect-error only serialized snake-case enum tags are valid
  kind: "future_variant",
  event_id: "$e",
  sender: "@a:s",
  body: "text",
  origin_server_ts: null,
};

const invalidName: RoomSummary = {
  // @ts-expect-error the Serde camelCase property is roomId
  room_id: "!r:s",
  displayName: null,
  unreadCount: 0,
  encryption: "encrypted",
  contact: null,
  pictureId: null,
};

const pictureWithUrl: RoomPicture = {
  pictureId: "0123456789abcdef",
  image: { handle: "h", width: 1, height: 1, bands: 1, bandRows: 1 },
  // @ts-expect-error a room picture is named by its ID, never by URL
  url: "mxc://s/m",
};

const unknownStore: AppStatus = {
  version: "0",
  revision: "unknown",
  milestone: "P1",
  // @ts-expect-error only the core's store codes are valid
  store: "plaintext",
  account,
};

const unknownAccountState: AppStatus = {
  version: "0",
  revision: "unknown",
  milestone: "P1",
  store: "protected",
  // @ts-expect-error only the core's account states are valid
  account: { state: "logged_in", userId: null, error: null },
};

const unknownNotice: TimelineItem = {
  kind: "notice",
  event_id: "$e",
  sender: "@a:s",
  // @ts-expect-error only the core's notice kinds are valid
  notice: "redacted",
  origin_server_ts: null,
};

const bigTime: TimelineItem = {
  kind: "message",
  event_id: "$e",
  sender: "@a:s",
  body: "text",
  // @ts-expect-error times are JavaScript numbers, not bigint
  origin_server_ts: 1n,
};

const bigImageSize: TimelineItem = {
  kind: "image",
  event_id: "$e",
  sender: "@a:s",
  origin_server_ts: null,
  name: "n",
  // @ts-expect-error sizes are JavaScript numbers, not bigint
  size: 1n,
  width: null,
  height: null,
  encrypted: true,
};

const unknownMime: ImagePick = {
  kind: "draft",
  preview: { handle: "h", width: 1, height: 1, bands: 1, bandRows: 1 },
  size: 1,
  // @ts-expect-error only PNG and JPEG are sent
  mime: "image/gif",
};

// @ts-expect-error the band height is required to place each band
const missingBandRows: ImageView = { handle: "h", width: 1, height: 1, bands: 1 };

const missingKind: PendingMessage = {
  localId: 1,
  roomId: "!r:s",
  // @ts-expect-error a pending message is text, an image, an edit or a file
  kind: "video",
  body: "",
  state: "queued",
  eventId: null,
  failure: null,
  queuedAt: 0,
};

const unknownChange: RoomChange = {
  // @ts-expect-error only the closed set of room changes is valid
  kind: "state",
  type: "m.room.server_acl",
};

const wrongChangeField: RoomChange = {
  kind: "kick",
  // @ts-expect-error the Serde camelCase property is userId
  user_id: "@b:s",
};

// @ts-expect-error the main address is required, though it may be null
const missingMainAddress: RoomChange = { kind: "canonical_alias", altAliases: [] };

// @ts-expect-error only the known join rules are valid
const unknownJoinRule: RoomSettings["joinRule"] = "open";

// @ts-expect-error the store state is required
const missingStore: AppStatus = { version: "0", revision: "unknown", milestone: "P1", account };

// @ts-expect-error the revision is required
const missingRevision: AppStatus = { version: "0", milestone: "P1", store: "protected", account };

void invalidTag;
void invalidName;
void unknownStore;
void missingStore;
void missingRevision;
void unknownAccountState;
void unknownNotice;
void bigTime;
void bigImageSize;
void unknownMime;
void missingBandRows;
void pictureWithUrl;
void missingKind;
void unknownChange;
void wrongChangeField;
void missingMainAddress;
void unknownJoinRule;

// @ts-expect-error nullable filter fields must be present
const missingFilter: LocalSearchFilters = { sender: null, fromMs: null };
const validFilters: LocalSearchFilters = { sender: null, fromMs: 0, beforeMs: null };
// @ts-expect-error filter timestamps use JavaScript numbers
const bigintFilter: LocalSearchFilters = { sender: null, fromMs: 1n, beforeMs: null };
// @ts-expect-error only the defined attachment kinds are accepted
const unknownAttachment: AttachmentKind = "videos";
// @ts-expect-error only the defined sync modes are accepted
const unknownSync: SyncMode = "polling";
// @ts-expect-error missing health observations must be explicit null
const missingHealth: ConnectionHealth = { accountState: "signed_out", storeState: "protected", syncMode: "unknown", consecutiveFailures: 0, retryDelayMs: 0, lastError: null };
// @ts-expect-error only the defined module scan states are accepted
const unknownScan: ModuleScanState = "done";
// @ts-expect-error only the defined module signatures are accepted
const unknownSignature: ModuleSignature = "signed";
// @ts-expect-error only the defined module processes are accepted
const unknownProcess: ModuleProcess = "webview";
// @ts-expect-error a missing signer must be explicit null
const missingSigner: ThirdPartyModule = { id: "a", fileName: "a.dll", path: "C:\a.dll", signature: "unsigned", processes: ["core"], firstSeen: 1, acknowledged: false };
// @ts-expect-error a missing scan time must be explicit null
const missingScan: ThirdPartyModules = { state: "pending", modules: [], truncated: false, incomplete: false };
const validModules: ThirdPartyModules = { state: "ready", scannedAt: null, modules: [{ id: "a", fileName: "a.dll", path: "C:\a.dll", signer: null, signature: "unsigned", processes: ["core", "web_view"], firstSeen: 1, acknowledged: false }], truncated: false, incomplete: false };
// @ts-expect-error the incomplete flag is required
const missingIncomplete: ThirdPartyModules = { state: "pending", scannedAt: null, modules: [], truncated: false };
void unknownScan;
void unknownSignature;
void unknownProcess;
void missingSigner;
void missingScan;
void missingIncomplete;
void validModules;
void missingFilter;
void validFilters;
void bigintFilter;
void unknownAttachment;
void unknownSync;
void missingHealth;
const idleCall: CallStatus = { phase: "idle", roomId: null, muted: false, participants: [], ended: null, joinError: null };
const readyCall: CallReadiness = { available: false, reason: "no_call_server", activeMembers: 0 };
// @ts-expect-error a status carries codes, never a token or URL
const callWithToken: CallStatus = { ...idleCall, token: "secret" };
// @ts-expect-error only the defined unavailable reasons are accepted
const unknownReason: CallUnavailable = "no_microphone";
// @ts-expect-error only the defined end codes are accepted
const unknownEnd: CallEnd = "crashed";
// @ts-expect-error only the defined join errors are accepted
const unknownJoin: CallJoinError = "busy";
const noDevices: CallDevices = { supported: true, recording: [{ id: "a", name: "Microphone" }], playout: [], selectedRecording: null, selectedPlayout: null, unreadable: false };
// @ts-expect-error a device list carries IDs and names, nothing else
const deviceWithPath: CallDevices = { ...noDevices, recording: [{ id: "a", name: "b", path: "c" }] };
// @ts-expect-error the saved choice must be explicit null, not missing
const missingChoice: CallDevices = { supported: true, recording: [], playout: [], selectedRecording: null, unreadable: false };
void noDevices;
void deviceWithPath;
void missingChoice;
void idleCall;
void readyCall;
void callWithToken;
void unknownReason;
void unknownEnd;
void unknownJoin;
