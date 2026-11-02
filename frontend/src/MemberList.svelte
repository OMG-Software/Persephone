<script lang="ts">
  import Icon from './Icon.svelte';
  import IconAction from './IconAction.svelte';
  import { memberMark, type MemberRow } from './members';
  import { avatarColour, initials } from './people';

  // The people in the selected room, down the right of the room. Each name
  // has a Verify icon while this account has not verified them, and a
  // verified icon once it has. Names and IDs are untrusted, isolated text.
  let {
    rows,
    truncated = false,
    failed = false,
    canVerify = false,
    verifying = false,
    asking = null,
    messageBusy = false,
    messageFailure = null,
    onverify,
    onmessage,
    oncancelask,
    onretry
  }: {
    // Null until the first list loads.
    rows: MemberRow[] | null;
    truncated?: boolean;
    // The last load failed.
    failed?: boolean;
    // This device may vouch for other people (it is verified itself).
    canVerify?: boolean;
    // A verification is starting or running.
    verifying?: boolean;
    // The member who has no direct conversation to verify in yet.
    asking?: string | null;
    messageBusy?: boolean;
    messageFailure?: string | null;
    onverify: (userId: string) => void;
    // Starts or opens a direct conversation with a member.
    onmessage: (userId: string) => void;
    oncancelask: () => void;
    onretry: () => void;
  } = $props();

  const markText = {
    verified: 'Verified',
    changed: 'Identity changed',
    no_identity: 'Has not set up encryption'
  } as const;
</script>

<aside class="member-panel" aria-labelledby="members-heading">
  <h2 id="members-heading">
    Members
    {#if rows}<span class="member-count">{rows.length}{truncated ? '+' : ''}</span>{/if}
  </h2>
  {#if rows && !canVerify}
    <p class="note">Verify this device to verify other people.</p>
  {/if}
  {#if failed}
    <p class="error" role="alert">
      The members could not be loaded.
      <button type="button" class="link" onclick={onretry}>Try again</button>
    </p>
  {/if}
  {#if rows === null}
    {#if !failed}<p class="note" role="status">Loading members…</p>{/if}
  {:else}
    <ul class="member-list">
      {#each rows as row (row.userId)}
        {@const mark = memberMark(row)}
        <li class="member">
          <span class="avatar small {avatarColour(row.userId)}" aria-hidden="true">{initials(row.userId)}</span>
          <span class="member-text">
            <bdi class="member-name">{row.name ?? row.userId}</bdi>
            {#if row.name !== null || row.invited}
              <span class="member-id"
                >{#if row.name !== null}<bdi>{row.userId}</bdi>{/if}{#if row.name !== null && row.invited}{' · '}{/if}{#if row.invited}Invited{/if}</span
              >
            {/if}
          </span>
          {#if mark === 'own'}
            <span class="member-own">You</span>
          {:else if mark === 'verify'}
            <IconAction
              icon="verify"
              class="member-verify"
              label="Verify"
              tip="start"
              disabled={!canVerify || verifying}
              onclick={() => onverify(row.userId)}
            >
              <bdi>{row.name ?? row.userId}</bdi>
            </IconAction>
          {:else if mark === 'verified' || mark === 'changed' || mark === 'no_identity'}
            <span class="action-anchor tip-start">
              <span class="member-mark {mark}" role="img" aria-label={markText[mark]}>
                <Icon name={mark === 'verified' ? 'shield' : mark === 'changed' ? 'shield-alert' : 'shield-off'} />
              </span>
              <span class="action-tip" aria-hidden="true">{markText[mark]}</span>
            </span>
          {/if}
        </li>
        {#if asking === row.userId}
          <li class="member-ask" role="group" aria-label="Verify in a direct conversation">
            <p>
              People are verified in a direct conversation with just the two of you. Start one with
              <bdi>{row.name ?? row.userId}</bdi>, then verify them there.
            </p>
            <div class="actions">
              <button type="button" class="secondary" disabled={messageBusy} onclick={() => onmessage(row.userId)}>
                {messageBusy ? 'Opening…' : 'Start conversation'}
              </button>
              <button type="button" class="link" disabled={messageBusy} onclick={oncancelask}>Cancel</button>
            </div>
            {#if messageFailure}<p class="error" role="alert">{messageFailure}</p>{/if}
          </li>
        {/if}
      {/each}
    </ul>
    {#if truncated}
      <p class="note">Showing the first {rows.length} members.</p>
    {/if}
  {/if}
</aside>
