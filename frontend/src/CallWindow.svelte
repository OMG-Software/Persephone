<script module lang="ts">
  import { callVideoFrame } from './bridge';
  import { reportFullscreen, toggleFullscreen } from './fullscreen';
  import { VideoFetcher } from './video';

  // One fetcher for the window's tiles, as in CallVideo.svelte.
  const fetcher = new VideoFetcher(callVideoFrame, () => {});
</script>

<script lang="ts">
  import { onDestroy, onMount, tick, untrack } from 'svelte';
  import type { CallParticipantLevel, CallStatus, CallVideoTile } from '../../crates/contracts/generated/contracts';
  import {
    callCamera,
    callDeafen,
    callLeave,
    callMicrophone,
    callMute,
    callScreen,
    callStatus,
    callVolume,
    callWindowReady,
    setCallFullscreen,
    closeCallWindow,
    watchSession
  } from './bridge';
  import {
    CAMERA_TEXT,
    cameraButton,
    DEAFEN_LABEL,
    DEVICE_CHANGE_TEXT,
    DEVICES_BUTTON_LABEL,
    END_TEXT,
    EXIT_FULLSCREEN_LABEL,
    feedLabel,
    FULLSCREEN_LABEL,
    IDLE_CALL,
    inCall,
    levelMap,
    deviceKey,
    MIC_GAIN_CHANGE_TEXT,
    MIC_GAIN_LABEL,
    MIC_GAIN_MAX,
    MIC_GAIN_STEP,
    micGainText,
    micGainValue,
    NO_KEY_TEXT,
    OWN_TILE_LABEL,
    PARTICIPANT_DEAFENED_LABEL,
    PARTICIPANT_MUTED_LABEL,
    PHASE_TEXT,
    SCREEN_TILE_LABEL,
    screenButton,
    screenNote,
    SHARING_TEXT,
    SHOW_ALL_LABEL,
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
  import { callGrid, ROSTER_LABEL, SelfClose, statsStayOpen, type CallCell } from './callWindow';
  import { AUTO_PICK, feedKeys, keepPick, largeKey, selectFeed, showAll, splitFeeds, type ViewPick } from './callView';
  import CallWindowDevices from './CallWindowDevices.svelte';
  import CallStatsPanel from './CallStats.svelte';
  import { STATS_LABEL } from './callStats';
  import { platformOf } from './modules';
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
  // Only the macOS screen refusal names a setting to change.
  const platform = platformOf(typeof navigator !== 'undefined' ? navigator.platform : '');
  let failure = $state<string | null>(null);
  let volumeDraft = $state<number | null>(null);
  let gainDraft = $state<number | null>(null);
  // The viewer's pick of a large feed (ADR-0065) and the device panel.
  let pick = $state.raw<ViewPick>(AUTO_PICK);
  let devicesOpen = $state(false);
  let devicesButton = $state<HTMLButtonElement | undefined>();
  // The technical details panel (ADR-0066); it closes when the call ends.
  let statsOpen = $state(false);
  let statsButton = $state<HTMLButtonElement>();
  // Numbers the status reads, so an older answer never replaces a newer one.
  let request = 0;
  // The window closes itself after a call it saw ends, unless it is
  // brought to the front in that time.
  const selfClose = new SelfClose(() => void closeCallWindow().catch(() => {}));

  function take(next: CallStatus) {
    if (!statsStayOpen(status, next)) statsOpen = false;
    status = next;
    selfClose.status(inCall(next), next.phase === 'ended');
    if (next.phase !== 'active') {
      ownLevel = 0;
      levels = new Map();
    }
    if (!inCall(next)) {
      pick = AUTO_PICK;
      devicesOpen = false;
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

  // The gain goes with the call's own automatic level flag, read from the
  // newest status when the slider moves, never from an older copy.
  async function setGain(gain: number) {
    gainDraft = gain;
    const mine = ++request;
    try {
      const next = await callMicrophone(status.autoLevel, gain);
      if (gainDraft === gain) gainDraft = null;
      if (mine === request) take(next);
    } catch {
      if (gainDraft === gain) gainDraft = null;
      failure = MIC_GAIN_CHANGE_TEXT;
    }
  }

  // The element that fills the screen, if any (full screen of a screen
  // share). The page learns of a change from the browser, also after Escape
  // or when the share ends.
  let fullscreenKey = $state<string | null>(null);
  function syncFullscreen() {
    const tile = document.fullscreenElement;
    fullscreenKey = tile instanceof HTMLElement ? (tile.dataset.cell ?? null) : null;
    // The host turns the window full screen on Windows; Escape, the exit
    // button and the end of the share all come here and send false.
    void reportFullscreen(document, setCallFullscreen);
  }
  onMount(() => {
    document.addEventListener('fullscreenchange', syncFullscreen);
    return () => document.removeEventListener('fullscreenchange', syncFullscreen);
  });

  // The first status read: when it works, the page can show and run the
  // call, so it tells the host it is ready (ADR-0060). A failure leaves it
  // unready, and the main window then shows the call's controls itself.
  async function start() {
    const mine = ++request;
    const next = await callStatus();
    if (mine === request) take(next);
    await callWindowReady();
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
      .then(start)
      .catch(() => (failure = 'The call window could not reach Persephone.'));
  });

  // Drawing: one canvas per video tile, as in CallVideo.svelte.
  type Shape = { width: number; height: number; rotation: number };
  const canvases: Record<number, HTMLCanvasElement | undefined> = {};
  let shapes = $state<Record<number, Shape | undefined>>({});

  // The newest picture of each tile, with the feed it belongs to. A feed
  // that moves between the large view, the row and the grid gets a new
  // canvas; it shows this at once. A tile that another feed took next shows
  // nothing until that feed's own picture comes.
  const latest: Record<number, { feed: string; picture: VideoPicture } | undefined> = {};
  // The feed each tile was last asked for, so a picture is kept with it.
  const asked: Record<number, string | undefined> = {};
  const tileKey = (tile: CallVideoTile) =>
    tile.own ? 'own' : tile.participant ? deviceKey(tile.participant.userId, tile.participant.deviceId) : 'none';
  const tileFeed = (tile: CallVideoTile) => `${tileKey(tile)} ${tile.stream}`;

  function show(tile: number, picture: VideoPicture) {
    const feed = asked[tile];
    latest[tile] = feed === undefined ? undefined : { feed, picture };
    draw(tile, picture);
  }

  function draw(tile: number, picture: VideoPicture) {
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
    latest[tile] = undefined;
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
  // Only a cell with a picture is a feed: a camera that goes off drops its pick.
  const keys = $derived(feedKeys(grid));
  const large = $derived(largeKey(pick, keys));
  const view = $derived(splitFeeds(grid, large));

  // A feed that went away drops its pick.
  $effect(() => {
    const kept = keepPick(pick, keys);
    if (kept !== pick) pick = kept;
  });

  // The tile that moves loses its button, so the focus follows the feed.
  async function choose(next: ViewPick, focusKey: string | null) {
    pick = next;
    await tick();
    if (focusKey === null) return;
    for (const node of document.querySelectorAll<HTMLButtonElement>('[data-feed]')) {
      if (node.dataset.feed === focusKey) {
        node.focus();
        return;
      }
    }
  }

  function closeDevices() {
    devicesOpen = false;
    void tick().then(() => devicesButton?.focus());
  }

  // One panel at a time: the technical details lie over the window's side,
  // so they would hide part of the device panel, and Escape closes either.
  function openDevices() {
    statsOpen = false;
    devicesOpen = true;
  }

  function openStats() {
    devicesOpen = false;
    statsOpen = true;
  }

  function onKeydown(event: KeyboardEvent) {
    if (event.key === 'Escape' && devicesOpen) {
      event.preventDefault();
      closeDevices();
    }
  }

  $effect(() => {
    const next: TileRef[] = status.video.map((tile) => ({ tile: tile.tile, key: tileKey(tile), stream: tile.stream }));
    untrack(() => {
      for (const key of Object.keys(asked)) delete asked[Number(key)];
      for (const tile of status.video) asked[tile.tile] ??= tileFeed(tile);
      fetcher.setTiles(next);
    });
  });

  $effect(() => {
    ticks;
    untrack(() => fetcher.poke());
  });

  // A kept cell can move to another tile, so the canvas follows its tile.
  const canvasRef = (node: HTMLCanvasElement, tile: number) => {
    const registration = tileCanvas(canvases, node, tile);
    // Only a picture of the feed now on the tile.
    const repaint = (at: number) => {
      const kept = latest[at];
      const now = untrack(() => status.video.find((tile) => tile.tile === at));
      if (kept && now && kept.feed === tileFeed(now)) draw(at, kept.picture);
    };
    repaint(tile);
    return {
      update(next: number) {
        registration.update(next);
        repaint(next);
      },
      destroy: registration.destroy
    };
  };

  const joined = $derived(inCall(status));
  const camera = $derived(cameraButton(status.camera));
  const screen = $derived(screenButton(status.screen));
  const volume = $derived(volumeDraft ?? status.volume);
  const gain = $derived(gainDraft ?? status.micGain);
</script>

{#snippet feed(cell: CallCell, isLarge: boolean)}
  {@const shape = cell.tile === null ? undefined : shapes[cell.tile]}
  {@const layout = shape && cell.tile !== null ? tileLayout(shape.width, shape.height, shape.rotation, cell.own && status.mirror) : null}
  <div class="video-tile call-cell" data-cell={cell.key} class:screen={cell.screen} class:speaking={cell.speaking} class:large={isLarge} style:--ratio={layout ? String(layout.ratio) : '1.7778'}>
    <button
      type="button"
      class="call-feed"
      data-feed={cell.key}
      aria-pressed={isLarge}
      disabled={cell.tile === null}
      title={cell.tile === null ? undefined : feedLabel(isLarge)}
      onclick={() => void choose(selectFeed(pick, cell.key, keys), cell.key)}
    >
      <!-- The action leads the button's name; the person's name follows. -->
      {#if cell.tile !== null}<span class="visually-hidden">{feedLabel(isLarge)}: </span>{/if}
      <span class="video-frame" style:aspect-ratio={layout ? String(layout.ratio) : '16 / 9'}>
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
      </span>
      <span class="video-name">
        {#if cell.own}
          {OWN_TILE_LABEL}
        {:else if cell.userId !== null}
          <PersonName userId={cell.userId} />
        {/if}
        {#if cell.screen}<span class="note">{SCREEN_TILE_LABEL}</span>{/if}
        {#if cell.muted}<span class="call-muted" role="img" aria-label={PARTICIPANT_MUTED_LABEL}><Icon name="mic-off" /></span>{/if}
        {#if cell.deafened}<span class="call-deafened" role="img" aria-label={PARTICIPANT_DEAFENED_LABEL}><Icon name="headphones-off" /></span>{/if}
      </span>
    </button>
    {#if cell.screen && cell.tile !== null}
      {@const full = fullscreenKey === cell.key}
      <button
        type="button"
        class="secondary call-fullscreen"
        aria-label={full ? EXIT_FULLSCREEN_LABEL : FULLSCREEN_LABEL}
        title={full ? EXIT_FULLSCREEN_LABEL : FULLSCREEN_LABEL}
        onclick={(event) => void toggleFullscreen(document, event.currentTarget.closest<HTMLElement>('.video-tile')!)}
      >
        <Icon name={full ? 'minimize' : 'maximize'} />
      </button>
    {/if}
  </div>
{/snippet}

<svelte:window onfocus={() => selfClose.shown()} onkeydown={onKeydown} />

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
    {#if joined && view.large}
      <button type="button" class="secondary call-show-all" onclick={() => void choose(showAll(), view.large?.key ?? null)}>
        {SHOW_ALL_LABEL}
      </button>
    {/if}
  </header>

  <div class="call-window-body">
    {#if joined}
      <section class="call-stage" aria-label="Call video">
        {#if view.large}
          <div class="call-large">{@render feed(view.large, true)}</div>
          <div class="call-strip">
            {#each view.rest as cell (cell.key)}
              {@render feed(cell, false)}
            {/each}
          </div>
        {:else}
          <div class="call-grid">
            {#each view.rest as cell (cell.key)}
              {@render feed(cell, false)}
            {/each}
          </div>
        {/if}
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
            {#if status.deafened}<span class="call-deafened" role="img" aria-label={PARTICIPANT_DEAFENED_LABEL}><Icon name="headphones-off" /></span>{/if}
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
            {#if person.deafened}<span class="call-deafened" role="img" aria-label={PARTICIPANT_DEAFENED_LABEL}><Icon name="headphones-off" /></span>{/if}
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
    {#if devicesOpen}
      <CallWindowDevices change={status.deviceChange} effectRefused={status.effectRefused} />
    {/if}
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
      <div class="call-levels">
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
        <label class="call-gain">
          <Icon name="mic" />
          <span>{MIC_GAIN_LABEL}</span>
          <input
            type="range"
            min="0"
            max={MIC_GAIN_MAX}
            step={MIC_GAIN_STEP}
            value={gain}
            aria-valuetext={micGainText(gain)}
            onchange={(event) => void setGain(micGainValue(Number(event.currentTarget.value)))}
          />
          <output class="note">{micGainText(gain)}</output>
        </label>
      </div>
      <button
        type="button"
        class="secondary call-devices-button"
        bind:this={devicesButton}
        aria-expanded={devicesOpen}
        aria-controls="call-window-devices"
        aria-label={DEVICES_BUTTON_LABEL}
        title={DEVICES_BUTTON_LABEL}
        onclick={() => (devicesOpen ? closeDevices() : openDevices())}
      >
        <Icon name="settings" />
      </button>
      <button
        type="button"
        class="secondary call-stats-button"
        aria-expanded={statsOpen}
        aria-controls="call-window-stats"
        aria-label={STATS_LABEL}
        title={STATS_LABEL}
        bind:this={statsButton}
        onclick={() => (statsOpen ? (statsOpen = false) : openStats())}
      >
        <Icon name="activity" />
      </button>
      <button type="button" class="call-leave danger" disabled={busy} onclick={() => command(callLeave, 'The call could not be left. Try again.')}>Leave</button>
    </footer>
    {#if CAMERA_TEXT[status.camera] !== ''}
      <p class="note call-window-note" role="status">{CAMERA_TEXT[status.camera]}</p>
    {/if}
    {#if screenNote(status.screen, platform) !== ''}
      <p class="note call-window-note" role="status">{screenNote(status.screen, platform)}</p>
    {/if}
    {#if status.deviceChange && !devicesOpen}
      <p class="note call-window-note" role="status">{DEVICE_CHANGE_TEXT[status.deviceChange]}</p>
    {/if}
  {/if}
  {#if failure}
    <p class="error call-window-note" role="alert">{failure}</p>
  {/if}
  {#if joined && statsOpen}
    <CallStatsPanel
      onclose={() => {
        statsOpen = false;
        void tick().then(() => statsButton?.focus());
      }}
    />
  {/if}
</main>
