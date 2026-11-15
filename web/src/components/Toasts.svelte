<script>
  import { toasts, dismiss } from '../lib/toast.svelte.js';
</script>

<div class="toasts" role="status" aria-live="polite">
  {#each toasts as t (t.id)}
    <div class="toast {t.kind}">
      <span>{t.message}</span>
      <button class="quiet small" aria-label="閉じる" onclick={() => dismiss(t.id)}>×</button>
    </div>
  {/each}
</div>

<style>
  .toasts {
    position: fixed;
    right: 1rem;
    bottom: 1rem;
    display: grid;
    gap: 0.5rem;
    z-index: 50;
    max-width: min(420px, calc(100vw - 2rem));
  }
  .toast {
    display: flex;
    gap: 0.6rem;
    align-items: start;
    justify-content: space-between;
    padding: 0.65rem 0.8rem 0.65rem 1rem;
    background: var(--ink);
    color: var(--panel);
    border-radius: var(--radius-m);
    border-left: 4px solid var(--matsu);
    font-size: 0.92rem;
    box-shadow: 0 6px 20px rgba(20, 25, 30, 0.25);
  }
  .toast.bad {
    border-left-color: var(--shu);
  }
  .toast button {
    color: inherit;
  }
</style>
