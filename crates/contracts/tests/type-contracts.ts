import type { RoomSummary, TimelineItem } from "../generated/contracts.js";

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

void invalidTag;
void invalidName;
