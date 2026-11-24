<script module lang="ts">
  import { callVideoFrame } from './bridge';
  import { VideoFetcher } from './video';

  // One fetcher for the window's tiles, as in CallVideo.svelte.
  const fetcher = new VideoFetcher(callVideoFrame, () => {});
</script>

<script lang="ts">
  import { onDestroy, onMount, untrack } from 'svelte';
  import type { CallParticipantLevel, CallStatus } from '../../crates/contracts/generated/contracts';
  import {
    callCamera,
    callDeafen,
    callLeave,
    callMute,
    callScreen,
    callStatus,
    callVolume,
    closeCallWindow,
    watchSession
  } from './bridge';
  import {
    CAMERA_TEXT,
    cameraButton,
    DEAFEN_LABEL,
    DEVICE_CHANGE_TEXT,
    END_TEXT,
    IDLE_CALL,
    inCall,
    levelMap,
    deviceKey,
    NO_KEY_TEXT,
    OWN_TILE_LABEL,
    PARTICIPANT_MUTED_LABEL,
    PHASE_TEXT,
    SCREEN_TEXT,
    SCREEN_TILE_LABEL,
    screenButton,
    SHARING_TEXT,
    SOUND_LEVEL_LABEL,
    SOUND_LEVEL_LOUD,
    SOUND_LEVEL_MAX,
    soundLevelText,
    UNDEAFEN_LABEL,
    UNREACHABLE_TEXT,
    VOLUME_LABEL,
    VOLUME_MAX,
    volumeText,
    volumeValue
  } from './calls';
  import { callGrid, ROSTER_LABEL, SelfClose } from './callWindow';
  import Icon from './Icon.svelte';
  import PersonName from './PersonName.svelte';
  import { tileCanvas, tileLayout, type TileRef, type VideoPicture } from './video';

  // The call window (ADR-0060): a grid with a tile for you and for each other
  // device in the call, its camera picture or a placeholder, and a list of
  // the participants down the right side that can be collapsed, with each
  // one's level meter and a mark when their microphone is off. It may call
  // only the call commands; it has no rooms, names or keys. User IDs are
  // untrusted text and show as plain text. The main window keeps its call bar.
  // `initialStatus` is what shows before the first status read; the page
  // mounts with none, so it starts idle.
  let { initialStatus = IDLE_CALL }: { initialStatus?: CallStatus } = $props();
  let status = $state<CallStatus>(untrack(() => initialStatus));
  let ownLevel = $state(0);
  let levels = $state<ReadonlyMap<string, number>>(new Map());
  let ticks = $state(0);
  let rosterOpen = $state(true);
  let busy = $state(false);
  let failure = $state<string | null>(null);
  let volumeDraft = $state<number | null>(null);
  // Numbers the status reads, so an older answer never replaces a newer one.
  let request = 0;
  // The window closes itself after a call it saw ends, unless it is
  // brought to the front in that time.
  const selfClose = new SelfClose(() => void closeCallWindow().catch(() => {}));

  function take(next: CallStatus) {
    status = next;
    selfClose.status(inCall(next));
    if (next.phase !== 'active') {
      ownLevel = 0;
      levels = new Map();
    }
  }

  async function refresh() {
    const mine = ++request;
    try {
      const next = await callStatus();
      if (mine === request) take(next);
    } catch {
      // The window keeps what it shows; the next change reads again.
    }
  }

  // Runs one call command and shows its reply; a failure shows a note.
  async function command(run: () => Promise<CallStatus>, failed: string) {
    if (busy) return;
    busy = true;
    failure = null;
    const mine = ++request;
    try {
      const next = await run();
      if (mine === request) take(next);
    } catch {
      failure = failed;
    } finally {
      busy = false;
    }
  }

  async function setVolume(percent: number) {
    volumeDraft = percent;
    const mine = ++request;
    try {
      const next = await callVolume(percent);
      if (volumeDraft === percent) volumeDraft = null;
      if (mine === request) take(next);
    } catch {
      if (volumeDraft === percent) volumeDraft = null;
      failure = 'The call volume could not be changed.';
    }
  }

  function takeLevels(next: CallParticipantLevel[]) {
    levels = status.phase === 'active' ? levelMap(next) : new Map();
  }

  onMount(() => {
    const nothing = () => {};
    void watchSession(
      refresh,
      nothing,
      nothing,
      nothing,
      refresh,
      () => (ticks += 1),
      nothing,
      nothing,
      (level) => (ownLevel = status.phase === 'active' ? level : 0),
      takeLevels
    )
      .then(refresh)
      .catch(() => (failure = 'The call window could not reach Persephone.'));
  });

  // Drawing: one canvas per video tile, as in CallVideo.svelte.
  type Shape = { width: number; height: number; rotation: number };
  const canvases: Record<number, HTMLCanvasElement | undefined> = {};
  let shapes = $state<Record<number, Shape | undefined>>({});

  function show(tile: number, picture: VideoPicture) {
    const canvas = canvases[tile];
    const context = canvas?.getContext('2d');
    if (!canvas || !context) return;
    if (canvas.width !== picture.width) canvas.width = picture.width;
    if (canvas.height !== picture.height) canvas.height = picture.height;
    context.putImageData(new ImageData(picture.pixels, picture.width, picture.height), 0, 0);
    const shape = shapes[tile];
    if (!shape || shape.width !== picture.width || shape.height !== picture.height || shape.rotation !== picture.rotation) {
      shapes[tile] = { width: picture.width, height: picture.height, rotation: picture.rotation };
    }
  }

  function clear(tile: number) {
    const canvas = canvases[tile];
    canvas?.getContext('2d')?.clearRect(0, 0, canvas.width, canvas.height);
    shapes[tile] = undefined;
  }

  fetcher.attach(show, clear);
  onDestroy(() => {
    fetcher.detach();
    selfClose.stop();
  });

  const grid = $derived(callGrid(status));

  $effect(() => {
    const next: TileRef[] = status.video.map((tile) => ({
      tile: tile.tile,
      key: tile.own ? 'own' : tile.participant ? deviceKey(tile.participant.userId, tile.participant.deviceId) : 'none',
      stream: tile.stream
    }));
    untrack(() => fetcher.setTiles(next));
  });

  $effect(() => {
    ticks;
    untrack(() => fetcher.poke());
  });

  // A kept cell can move to another tile, so the canvas follows its tile.
  const canvasRef = (node: HTMLCanvasElement, tile: number) => tileCanvas(canvases, node, tile);

  const joined = $derived(inCall(status));
  const camera = $derived(cameraButton(status.camera));
  const screen = $derived(screenButton(status.screen));
  const volume = $derived(volumeDraft ?? status.volume);
</script>

<svelte:window onfocus={() => selfClose.shown()} />

<main class="call-window">
  <header class="call-window-head">
    <Icon name="phone" />
    <p class="call-phase">{PHASE_TEXT[status.phase]}</p>
    <button
      type="button"
      class="secondary call-roster-toggle"
      aria-expanded={rosterOpen}
      aria-controls="call-roster"
      onclick={() => (rosterOpen = !rosterOpen)}
    >
      <Icon name={rosterOpen ? 'chevron-right' : 'chevron-left'} />
      {rosterOpen ? `Hide ${ROSTER_LABEL.toLowerCase()}` : `Show ${ROSTER_LABEL.toLowerCase()}`}
    </button>
  </header>

  <div class="call-window-body">
    {#if joined}
      <section class="call-grid" aria-label="Call video">
        {#each grid as cell (cell.key)}
          {@const shape = cell.tile === null ? undefined : shapes[cell.tile]}
          {@const layout = shape && cell.tile !== null ? tileLayout(shape.width, shape.height, shape.rotation, cell.own) : null}
          <figure class="video-tile call-cell" class:screen={cell.screen} class:speaking={cell.speaking}>
            <div class="video-frame" style:aspect-ratio={layout ? String(layout.ratio) : '16 / 9'}>
              {#if cell.tile !== null}
                <canvas
                  use:canvasRef={cell.tile}
                  class="video-canvas"
                  hidden={layout === null}
                  aria-hidden="true"
                  style:width={layout ? `${layout.canvasPercent}%` : undefined}
                  style:transform={layout?.transform}
                ></canvas>
              {/if}
              {#if layout === null}
                <span class="call-placeholder" aria-hidden="true"><Icon name="video-off" /></span>
              {/if}
            </div>
            <figcaption class="video-name">
              {#if cell.own}
                {OWN_TILE_LABEL}
              {:else if cell.userId !== null}
                <PersonName userId={cell.userId} />
              {/if}
              {#if cell.screen}<span class="note">{SCREEN_TILE_LABEL}</span>{/if}
              {#if cell.muted}<span class="call-muted" role="img" aria-label={PARTICIPANT_MUTED_LABEL}><Icon name="mic-off" /></span>{/if}
            </figcaption>
          </figure>
        {/each}
      </section>
    {:else}
      <section class="call-grid-empty">
        <p>{status.phase === 'ended' && status.ended !== null ? END_TEXT[status.ended] : 'No call is running.'}</p>
        <button type="button" class="secondary" onclick={() => void closeCallWindow().catch(() => {})}>Close</button>
      </section>
    {/if}

    <aside id="call-roster" class="call-roster" hidden={!rosterOpen} aria-label={ROSTER_LABEL}>
      <h2>{ROSTER_LABEL}</h2>
      <ul>
        {#if joined}
          <li>
            <span class="call-roster-name">{OWN_TILE_LABEL}</span>
            {#if status.muted}<span class="call-muted" role="img" aria-label={PARTICIPANT_MUTED_LABEL}><Icon name="mic-off" /></span>{/if}
            <div
              class="call-level"
              class:loud={ownLevel >= SOUND_LEVEL_LOUD}
              role="meter"
              aria-label={SOUND_LEVEL_LABEL}
              aria-valuemin="0"
              aria-valuemax={SOUND_LEVEL_MAX}
              aria-valuenow={ownLevel}
              aria-valuetext={soundLevelText(ownLevel)}
            >
              <span class="call-level-fill" style:transform="scaleX({ownLevel / SOUND_LEVEL_MAX})"></span>
            </div>
          </li>
        {/if}
        {#each status.participants as person, index (deviceKey(person.userId, person.deviceId))}
          {@const level = levels.get(deviceKey(person.userId, person.deviceId)) ?? 0}
          {@const nameId = `call-roster-name-${index}`}
          <li>
            <span class="call-roster-name" id={nameId}><PersonName userId={person.userId} /></span>
            {#if person.muted}<span class="call-muted" role="img" aria-label={PARTICIPANT_MUTED_LABEL}><Icon name="mic-off" /></span>{/if}
            <div
              class="call-level"
              class:loud={level >= SOUND_LEVEL_LOUD}
              role="meter"
              aria-labelledby={nameId}
              aria-valuemin="0"
              aria-valuemax={SOUND_LEVEL_MAX}
              aria-valuenow={level}
              aria-valuetext={soundLevelText(level)}
            >
              <span class="call-level-fill" style:transform="scaleX({level / SOUND_LEVEL_MAX})"></span>
            </div>
            {#if person.sharing}<span class="note">{SHARING_TEXT}</span>{/if}
            {#if !person.keyed}<span class="note">{NO_KEY_TEXT}</span>{/if}
            {#if !person.reachable}<span class="note">{UNREACHABLE_TEXT}</span>{/if}
          </li>
        {/each}
      </ul>
    </aside>
  </div>

  {#if joined}
    <footer class="call-window-controls">
      <button type="button" class="secondary" aria-pressed={status.muted} disabled={busy} onclick={() => command(() => callMute(!status.muted), 'The microphone did not change. Try again.')}>
        <Icon name={status.muted ? 'mic-off' : 'mic'} />
        {status.muted ? 'Unmute' : 'Mute'}
      </button>
      <button type="button" class="secondary" aria-pressed={status.deafened} disabled={busy} onclick={() => command(() => callDeafen(!status.deafened), 'The call sound did not change. Try again.')}>
        <Icon name={status.deafened ? 'headphones-off' : 'headphones'} />
        {status.deafened ? UNDEAFEN_LABEL : DEAFEN_LABEL}
      </button>
      <button type="button" class="secondary call-camera" aria-pressed={camera.pressed} disabled={busy || status.phase !== 'active'} onclick={() => command(() => callCamera(!camera.pressed), 'The camera did not change. Try again.')}>
        <Icon name={camera.pressed ? 'video-off' : 'video'} />
        {camera.label}
      </button>
      <button type="button" class="secondary call-screen" aria-pressed={screen.pressed} disabled={busy || status.phase !== 'active'} onclick={() => command(() => callScreen(!screen.pressed), 'The screen share did not change. Try again.')}>
        <Icon name="screen" />
        {screen.label}
      </button>
      <label class="call-volume">
        <Icon name="volume" />
        <span>{VOLUME_LABEL}</span>
        <input
          type="range"
          min="0"
          max={VOLUME_MAX}
          step="5"
          value={volume}
          aria-valuetext={volumeText(volume)}
          onchange={(event) => void setVolume(volumeValue(Number(event.currentTarget.value)))}
        />
        <output class="note">{volumeText(volume)}</output>
      </label>
      <button type="button" class="call-leave danger" disabled={busy} onclick={() => command(callLeave, 'The call could not be left. Try again.')}>Leave</button>
    </footer>
    {#if CAMERA_TEXT[status.camera] !== ''}
      <p class="note call-window-note" role="status">{CAMERA_TEXT[status.camera]}</p>
    {/if}
    {#if SCREEN_TEXT[status.screen] !== ''}
      <p class="note call-window-note" role="status">{SCREEN_TEXT[status.screen]}</p>
    {/if}
    {#if status.deviceChange}
      <p class="note call-window-note" role="status">{DEVICE_CHANGE_TEXT[status.deviceChange]}</p>
    {/if}
  {/if}
  {#if failure}
    <p class="error call-window-note" role="alert">{failure}</p>
  {/if}
</main>
