<script lang="ts">
  import type { CallStatus } from '../../crates/contracts/generated/contracts';
  import { callAnnouncement, END_TEXT, NO_KEY_TEXT, PHASE_TEXT, showCallBar, UNREACHABLE_TEXT } from './calls';
  import Icon from './Icon.svelte';
  import PersonName from './PersonName.svelte';

  // The call, across every room. Names and room IDs are untrusted text; the
  // core's codes show as fixed texts from calls.ts. The live region stays in
  // the page so that a change in it is announced. Speaking changes are not
  // announced: each participant's mark is read in place only.
  let {
    status,
    dismissed,
    roomName,
    names,
    busy,
    onmute,
    onleave,
    ondismiss
  }: {
    status: CallStatus;
    dismissed: boolean;
    roomName: string;
    // Display names from the call room's members, by user ID.
    names: ReadonlyMap<string, string>;
    // A mute or leave request is waiting for the core.
    busy: boolean;
    onmute: () => void;
    onleave: () => void;
    ondismiss: () => void;
  } = $props();

  const shown = $derived(showCallBar(status, dismissed));
  const ended = $derived(status.phase === 'ended');
</script>

<div class="visually-hidden" aria-live="polite" aria-atomic="true">
  {callAnnouncement(status, dismissed)}
</div>
{#if shown}
  <section class="call-bar" aria-label="Call">
    <div class="call-head">
      <Icon name="phone" />
      <p class="call-room"><bdi>{roomName}</bdi></p>
      <p class="call-phase note">{PHASE_TEXT[status.phase]}</p>
      <span class="call-controls">
        {#if ended}
          <button type="button" class="secondary call-dismiss" onclick={ondismiss}>Dismiss</button>
        {:else}
          <button
            type="button"
            class="secondary call-mute"
            aria-pressed={status.muted}
            disabled={busy}
            onclick={onmute}
          >
            <Icon name={status.muted ? 'mic-off' : 'mic'} />
            {status.muted ? 'Unmute' : 'Mute'}
          </button>
          <button type="button" class="secondary call-leave" onclick={onleave}>Leave</button>
        {/if}
      </span>
    </div>
    {#if ended}
      <p class="call-end">{status.ended === null ? END_TEXT.internal : END_TEXT[status.ended]}</p>
    {:else if status.participants.length > 0}
      <ul class="call-people" aria-label="Call participants">
        {#each status.participants as person (person.userId + '/' + person.deviceId)}
          <li>
            {#if person.speaking}
              <span class="speaking-mark" role="img" aria-label="Speaking"></span>
            {:else}
              <span class="speaking-mark idle" aria-hidden="true"></span>
            {/if}
            <PersonName userId={person.userId} name={names.get(person.userId) ?? null} />
            {#if !person.keyed}<span class="note call-note">{NO_KEY_TEXT}</span>{/if}
            {#if !person.reachable}<span class="note call-note">{UNREACHABLE_TEXT}</span>{/if}
          </li>
        {/each}
      </ul>
    {/if}
  </section>
{/if}
