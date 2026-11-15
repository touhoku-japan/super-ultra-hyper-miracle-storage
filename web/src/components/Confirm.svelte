<script>
  // Confirmation for destructive actions. `confirmText` makes the user type a name first.
  import Modal from './Modal.svelte';
  import { toast } from '../lib/toast.svelte.js';
  import { errorMessage } from '../lib/s3.js';

  let {
    open = $bindable(false),
    title,
    message,
    action = '削除',
    confirmText = null,
    danger = true,
    onconfirm,
    children,
  } = $props();
  let typed = $state('');
  let busy = $state(false);

  $effect(() => {
    if (open) typed = '';
  });

  async function run() {
    busy = true;
    try {
      await onconfirm();
      open = false;
    } catch (e) {
      toast(errorMessage(e), 'bad');
    } finally {
      busy = false;
    }
  }
</script>

<Modal bind:open {title}>
  {#if message}<p>{message}</p>{/if}
  {@render children?.()}
  {#if confirmText}
    <label class="field">
      <span>確認のため <code>{confirmText}</code> と入力してください</span>
      <input type="text" bind:value={typed} autocomplete="off" />
    </label>
  {/if}
  {#snippet footer()}
    <button onclick={() => (open = false)}>キャンセル</button>
    <button
      class={danger ? 'danger solid' : 'primary'}
      disabled={busy || (confirmText && typed !== confirmText)}
      onclick={run}>{busy ? '実行中…' : action}</button
    >
  {/snippet}
</Modal>
