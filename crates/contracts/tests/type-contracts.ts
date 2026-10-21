import type { AppStatus, RoomSummary, TimelineItem } from "../generated/contracts.js";

const invalidTag: TimelineItem = {
  // @ts-expect-error only serialized snake-case enum tags are valid
  kind: "future_variant",
  event_id: "$e",
  sender: "@a:s",
  body: "text",
};

const invalidName: RoomSummary = {
  // @ts-expect-error the Serde camelCase property is roomId
  room_id: "!r:s",
  displayName: null,
  unreadCount: 0,
};

const unknownStore: AppStatus = {
  version: "0",
  milestone: "P1",
  // @ts-expect-error only the core's store codes are valid
  store: "plaintext",
};

// @ts-expect-error the store state is required
const missingStore: AppStatus = { version: "0", milestone: "P1" };

void invalidTag;
void invalidName;
void unknownStore;
void missingStore;
