<script>
  // A dialog on the native <dialog> element (focus trap and Esc come for free).
  let { title, open = $bindable(false), wide = false, onclose, children, footer } = $props();
  let dialog;

  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    if (!open && dialog.open) dialog.close();
  });
</script>

<dialog
  bind:this={dialog}
  class:wide
  onclose={() => {
    open = false;
    onclose?.();
  }}
>
  {#if open}
    <header>
      <h2>{title}</h2>
      <button class="quiet" aria-label="閉じる" onclick={() => (open = false)}>×</button>
    </header>
    <div class="content">{@render children?.()}</div>
    {#if footer}
      <footer>{@render footer()}</footer>
    {/if}
  {/if}
</dialog>

<style>
  dialog {
    border: 1px solid var(--line);
    border-radius: var(--radius-m);
    padding: 0;
    width: min(520px, calc(100vw - 2rem));
    background: var(--panel);
    color: var(--ink);
    box-shadow: 0 20px 60px rgba(15, 20, 25, 0.35);
  }
  dialog.wide {
    width: min(860px, calc(100vw - 2rem));
  }
  dialog::backdrop {
    background: rgba(25, 30, 36, 0.5);
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0.9rem 1.1rem;
    border-bottom: 1px solid var(--line);
  }
  .content {
    padding: 1.1rem;
    display: grid;
    gap: 1rem;
    max-height: 70vh;
    overflow-y: auto;
  }
  footer {
    display: flex;
    justify-content: flex-end;
    gap: 0.6rem;
    padding: 0.8rem 1.1rem;
    border-top: 1px solid var(--line);
    background: var(--wall);
  }
</style>
