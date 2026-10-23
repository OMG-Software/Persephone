import type { AppStatus, RoomSummary, TimelineItem } from "../generated/contracts.js";

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
};

const unknownStore: AppStatus = {
  version: "0",
  milestone: "P1",
  // @ts-expect-error only the core's store codes are valid
  store: "plaintext",
  account,
};

const unknownAccountState: AppStatus = {
  version: "0",
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

// @ts-expect-error the store state is required
const missingStore: AppStatus = { version: "0", milestone: "P1", account };

void invalidTag;
void invalidName;
void unknownStore;
void missingStore;
void unknownAccountState;
void unknownNotice;
void bigTime;
