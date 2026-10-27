import type {
  AppStatus,
  ImagePick,
  ImageView,
  PendingMessage,
  RoomChange,
  RoomSettings,
  RoomSummary,
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
  // @ts-expect-error a pending message says whether it is text or an image
  kind: "file",
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
void missingKind;
void unknownChange;
void wrongChangeField;
void missingMainAddress;
void unknownJoinRule;
