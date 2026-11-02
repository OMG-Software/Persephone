<script lang="ts">
  import { onDestroy } from 'svelte';
  import { joinPublicRoom, publicRooms, type PublicRoom } from './bridge';
  import FeatureDialog from './FeatureDialog.svelte';
  import {
    MAX_DIRECTORY_FILTER_BYTES, MAX_LISTED_ROOMS, MAX_SERVER_NAME_BYTES, addPage, filterProblem, joinFailure,
    listFailure, memberText, roomLabel, serverProblem, type Listing
  } from './publicRooms.ts';

  // Browses a public room directory: this account's homeserver's, or another
  // server's through the homeserver. Nothing is sent until Browse is pressed.
  // A listed room can be viewed, then joined, or opened when already joined.
  // `homeserver`: this account's server name, shown as the default.
  // `onopen`: open a joined room; `onjoined`: a room was just joined.
  let { homeserver = null, onopen, onjoined, onclose }: {
    homeserver?: string | null; onopen: (roomId: string) => void; onjoined: (roomId: string) => void;
    onclose: () => void;
  } = $props();

  // The pause before asking again while the core lists another page.
  const RETRY_MS = 400;
  const RETRIES = 10;

  let server = $state('');
  let filter = $state('');
  // The server and filter the shown listing is for.
  let asked = $state<{ server: string | null; filter: string | null } | null>(null);
  let listing = $state<Listing | null>(null);
  let loading = $state(false);
  let failure = $state<string | null>(null);
  let viewed = $state<PublicRoom | null>(null);
  let joining = $state(false);
  let joinError = $state<string | null>(null);
  // A reply is shown only if it answers the newest request, and only while open.
  let request = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let active = true;
  onDestroy(() => {
    active = false;
    request += 1;
    clearTimeout(timer);
  });

  const problem = $derived(serverProblem(server) ?? filterProblem(filter));

  /** Ask for one page; a busy core is asked again a few times. */
  function fetchPage(target: { server: string | null; filter: string | null }, since: string | null) {
    const mine = ++request;
    clearTimeout(timer);
    loading = true;
    failure = null;
    let tries = 0;
    const attempt = async () => {
      try {
        const page = await publicRooms(target.server, target.filter, since);
        if (!active || mine !== request) return;
        listing = addPage(since === null ? null : listing, page, since);
        asked = target;
        loading = false;
      } catch (reason) {
        if (!active || mine !== request) return;
        if (String(reason) === 'limit_exceeded' && tries < RETRIES) {
          tries += 1;
          timer = setTimeout(attempt, RETRY_MS);
          return;
        }
        failure = listFailure(String(reason), target.server !== null);
        loading = false;
      }
    };
    void attempt();
  }

  function browse(event: SubmitEvent) {
    event.preventDefault();
    if (problem || joining) return;
    viewed = null;
    joinError = null;
    listing = null;
    fetchPage({ server: server.trim() || null, filter: filter.trim() || null }, null);
  }

  function more() {
    if (!asked || !listing?.next || loading) return;
    fetchPage(asked, listing.next);
  }

  function view(room: PublicRoom) {
    viewed = room;
    joinError = null;
  }

  async function join(room: PublicRoom) {
    if (joining) return;
    joining = true;
    joinError = null;
    try {
      const joined = await joinPublicRoom(room.roomId);
      if (!active) return;
      onjoined(joined.roomId);
      onclose();
    } catch (reason) {
      if (active) joinError = joinFailure(String(reason));
    } finally {
      if (active) joining = false;
    }
  }

  function openJoined(room: PublicRoom) {
    onopen(room.roomId);
    onclose();
  }
</script>

<FeatureDialog title="Public rooms" {onclose} closeDisabled={joining}>
  <div class="public-rooms">
    <form class="public-rooms-search" onsubmit={browse}>
      <label for="public-rooms-server">Server</label>
      <input id="public-rooms-server" class="field" bind:value={server} maxlength={MAX_SERVER_NAME_BYTES}
        placeholder={homeserver ? `${homeserver} (your homeserver)` : 'Your homeserver'}
        spellcheck="false" autocomplete="off" disabled={joining} />
      <label for="public-rooms-filter">Search (optional)</label>
      <input id="public-rooms-filter" class="field" bind:value={filter} maxlength={MAX_DIRECTORY_FILTER_BYTES}
        spellcheck="false" autocomplete="off" disabled={joining} />
      <p class="note">
        Leave the server empty to browse your homeserver. Your homeserver sees the server name and the search text,
        and asks another server for its directory on your behalf.
      </p>
      {#if problem}<p role="alert" class="error">{problem}</p>{/if}
      <div class="actions">
        <button type="submit" class="primary" disabled={!!problem || loading || joining}>
          {loading && !listing ? 'Browsing…' : 'Browse'}
        </button>
      </div>
    </form>

    {#if failure}<p role="alert" class="error">{failure}</p>{/if}

    {#if listing}
      <p class="note" role="status">
        {listing.rooms.length === 0
          ? 'No public rooms found.'
          : `Showing ${listing.rooms.length}${listing.total !== null ? ` of about ${listing.total.toLocaleString('en')}` : ''} rooms`}
        {#if asked?.server} on <bdi>{asked.server}</bdi>{/if}.
      </p>
      {#if listing.rooms.length > 0}
        <ul class="public-room-list" aria-label="Public rooms">
          {#each listing.rooms as room (room.roomId)}
            <li>
              <button type="button" class="public-room-row" aria-current={viewed?.roomId === room.roomId ? 'true' : undefined}
                onclick={() => view(room)}>
                <bdi class="public-room-name">{roomLabel(room)}</bdi>
                {#if room.alias && room.name}<bdi class="public-room-alias">{room.alias}</bdi>{/if}
                <span class="public-room-members">{memberText(room.members)}{room.joined ? ' · Joined' : ''}</span>
              </button>
            </li>
          {/each}
        </ul>
      {/if}
      {#if listing.next}
        <button type="button" class="secondary" disabled={loading || joining} onclick={more}>
          {loading ? 'Loading…' : 'Show more rooms'}
        </button>
      {:else if listing.rooms.length >= MAX_LISTED_ROOMS}
        <p class="note">This list shows at most {MAX_LISTED_ROOMS} rooms. Search to narrow it.</p>
      {/if}
    {/if}

    {#if viewed}
      <section class="public-room-details" aria-labelledby="public-room-heading">
        <h3 id="public-room-heading"><bdi>{roomLabel(viewed)}</bdi></h3>
        <dl>
          {#if viewed.alias}<dt>Address (as listed)</dt><dd><bdi>{viewed.alias}</bdi></dd>{/if}
          <dt>Room ID</dt><dd><bdi>{viewed.roomId}</bdi></dd>
          <dt>Members</dt><dd>{memberText(viewed.members)}</dd>
          <dt>History</dt><dd>{viewed.worldReadable ? 'Anyone can read it without joining' : 'Only members can read it'}</dd>
          <dt>Guests</dt><dd>{viewed.guestsCanJoin ? 'Guests can join' : 'Guests cannot join'}</dd>
        </dl>
        {#if viewed.topic}<p class="public-room-topic"><bdi>{viewed.topic}</bdi></p>{/if}
        <p class="note">
          The directory supplies this name, address and topic, and nothing checks them. A room can claim an address
          it does not own.
        </p>
        {#if viewed.joined}
          <button type="button" class="primary" onclick={() => openJoined(viewed!)}>Open room</button>
        {:else if viewed.joinable}
          <p class="note">
            Public rooms are usually not encrypted. Other members and the servers in the room can read what you send,
            and Persephone asks before you send to a room that is not encrypted. Joining shows your Matrix ID to the
            room.
          </p>
          <button type="button" class="primary" disabled={joining} onclick={() => join(viewed!)}>
            {joining ? 'Joining…' : 'Join room'}
          </button>
        {:else}
          <p class="note">This room does not let anyone join. It may ask you to knock, or let in only invited people.</p>
        {/if}
        {#if joinError}<p role="alert" class="error">{joinError}</p>{/if}
      </section>
    {/if}
  </div>
</FeatureDialog>
