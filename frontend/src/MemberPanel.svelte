<script lang="ts">
  import { onDestroy, untrack } from 'svelte';
  import type {
    ContactStatus,
    RoomMember,
    RoomMembers,
    RoomReader,
    RoomReaders
  } from '../../crates/contracts/generated/contracts';
  import { roomMembers, roomReaders } from './bridge';
  import MemberList from './MemberList.svelte';
  import { memberRows } from './members';

  // Loads the selected room's members for the member list. The owner
  // remounts this panel when the room or session changes, so a reply always
  // belongs to this room; a newer request still wins over an older one.
  // The core runs one member, reader or server listing at a time; a few more
  // wait their turn, and beyond that or after a time limit it answers
  // `limit_exceeded`. The panel asks one after the other, leaves the core to
  // the room settings (which list them too) while they are open, and tries
  // again a little later when the core is busy.
  let {
    roomId,
    version,
    contact = null,
    paused = false,
    canVerify = false,
    verifying = false,
    asking = null,
    onverify,
    oncancelask,
    onmessage
  }: {
    roomId: string;
    // Changes when the room or trust may have changed: the list loads again.
    version: number;
    // The direct-message contact's status, which is more current than the
    // reader list's for that one person.
    contact?: ContactStatus | null;
    // Do not ask the core while true; load once it is false again.
    paused?: boolean;
    canVerify?: boolean;
    verifying?: boolean;
    // The member the core found no direct conversation with, to verify in:
    // the panel offers to start one.
    asking?: string | null;
    // Starts verifying a member; the core reports the outcome.
    onverify: (userId: string) => void;
    oncancelask: () => void;
    // Starts or opens a direct conversation; resolves with why it failed.
    onmessage: (userId: string) => Promise<string | null>;
  } = $props();

  let readers = $state<RoomReader[] | null>(null);
  let members = $state<RoomMember[] | null>(null);
  let truncated = $state(false);
  let failed = $state(false);
  let messageBusy = $state(false);
  let messageFailure = $state<string | null>(null);
  const rows = $derived(readers === null ? null : memberRows(readers, members, contact));

  // At most this many tries in a row while the core is busy, then the
  // panel says the load failed and offers Try again.
  const BUSY_TRIES = 5;
  const DELAY_MS = 1000;
  let request = 0;
  let active = true;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let loaded = false;
  let pending = false;
  let busyTries = 0;
  onDestroy(() => {
    active = false;
    clearTimeout(timer);
  });

  const isBusy = (reason: unknown) => String(reason) === 'limit_exceeded';

  /** Load after a short delay; several changes in that time load once. */
  function schedule() {
    if (timer !== undefined) return;
    timer = setTimeout(() => {
      timer = undefined;
      void load();
    }, DELAY_MS);
  }

  /** The core was busy: try again later, a bounded number of times. */
  function busy(asked: number) {
    if (!active || asked !== request) return;
    busyTries += 1;
    if (busyTries < BUSY_TRIES) schedule();
    else failed = true;
  }

  /** Read who is in the room and how far each is trusted, then display names (best effort). */
  async function load() {
    if (paused) {
      pending = true;
      return;
    }
    const asked = ++request;
    let found: RoomReaders;
    try {
      found = await roomReaders(roomId);
    } catch (reason) {
      if (isBusy(reason)) busy(asked);
      else if (active && asked === request) failed = true;
      return;
    }
    if (!active || asked !== request) return;
    readers = found.readers;
    truncated = found.truncated;
    failed = false;
    // Room settings opened meanwhile: leave the core to them, and read the
    // names when they close. Names the core does not list now stay as they were.
    if (paused) {
      pending = true;
      return;
    }
    let named: RoomMembers | null = null;
    let namesBusy = false;
    try {
      named = await roomMembers(roomId, '');
    } catch (reason) {
      namesBusy = isBusy(reason);
    }
    if (!active || asked !== request) return;
    if (named) members = named.members;
    if (namesBusy) busy(asked);
    else busyTries = 0;
  }

  // Load at once, then once more after the room or trust changes; a busy
  // room changes with every message, so changes close together load once.
  $effect(() => {
    void version;
    // Only `version` starts a load here; the pause has its own effect.
    untrack(() => {
      if (!loaded) {
        loaded = true;
        void load();
      } else {
        schedule();
      }
    });
  });

  // A load skipped while paused runs when the pause ends.
  $effect(() => {
    if (!paused && pending) {
      pending = false;
      schedule();
    }
  });

  /** Try again at once, from the start of the busy count. */
  function retry() {
    busyTries = 0;
    failed = false;
    void load();
  }

  function verify(userId: string) {
    messageFailure = null;
    onverify(userId);
  }

  async function message(userId: string) {
    if (messageBusy) return;
    messageBusy = true;
    messageFailure = null;
    const failure = await onmessage(userId);
    if (!active) return;
    messageBusy = false;
    messageFailure = failure;
  }
</script>

<MemberList
  {rows}
  {truncated}
  {failed}
  {canVerify}
  {verifying}
  {asking}
  {messageBusy}
  {messageFailure}
  onverify={verify}
  onmessage={message}
  oncancelask={() => {
    messageFailure = null;
    oncancelask();
  }}
  onretry={retry}
/>
