<script>
  // Page title row: breadcrumb trail, title, and actions on the right.
  let { title, crumbs = [], children } = $props();
</script>

<header class="head">
  <div>
    {#if crumbs.length}
      <nav class="crumbs" aria-label="現在地">
        {#each crumbs as c, i (i)}
          {#if c.href}<a href={c.href}>{c.label}</a>{:else}<span>{c.label}</span>{/if}
          {#if i < crumbs.length - 1}<span class="sep" aria-hidden="true">/</span>{/if}
        {/each}
      </nav>
    {/if}
    <h1 class="break">{title}</h1>
  </div>
  {#if children}<div class="row">{@render children()}</div>{/if}
</header>

<style>
  .head {
    display: flex;
    justify-content: space-between;
    align-items: end;
    gap: 1rem;
    flex-wrap: wrap;
    margin-bottom: 1.4rem;
  }
  .crumbs {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
    font-size: 0.88rem;
    color: var(--ink-faint);
    margin-bottom: 0.3rem;
  }
  .crumbs a {
    color: var(--ink-soft);
  }
  .sep {
    color: var(--lattice);
  }
</style>
