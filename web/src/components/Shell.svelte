<script>
  // Sidebar (the lattice) + main column. `nav` is [{ href, label, active }].
  let { product, who, nav, onlogout, children } = $props();
  let menuOpen = $state(false);
</script>

<div class="shell">
  <aside class="lattice" class:open={menuOpen}>
    <div class="brand">
      <span class="mark" aria-hidden="true"></span>
      <div>
        <strong>SUHMS</strong>
        <span>{product}</span>
      </div>
      <button class="quiet menu" aria-label="メニュー" aria-expanded={menuOpen} onclick={() => (menuOpen = !menuOpen)}>☰</button>
    </div>
    <nav>
      {#each nav as item (item.href)}
        <a href={item.href} aria-current={item.active ? 'page' : undefined} onclick={() => (menuOpen = false)}>{item.label}</a>
      {/each}
    </nav>
    <div class="who">
      <span class="faint-side">ログイン中</span>
      <span class="name">{who}</span>
      <button class="quiet logout" onclick={onlogout}>ログアウト</button>
    </div>
  </aside>
  <main>
    {@render children()}
  </main>
</div>

<style>
  .shell {
    display: grid;
    grid-template-columns: 236px 1fr;
    min-height: 100vh;
  }
  aside {
    color: var(--side-ink);
    display: flex;
    flex-direction: column;
    padding: 1.2rem 0.9rem;
    gap: 1.6rem;
    position: sticky;
    top: 0;
    height: 100vh;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 0.7rem;
    padding: 0 0.4rem;
  }
  .brand div {
    display: grid;
    line-height: 1.25;
  }
  .brand strong {
    font-size: 1.05rem;
    letter-spacing: 0.12em;
  }
  .brand span {
    font-size: 0.8rem;
    color: var(--side-faint);
  }
  .mark {
    width: 26px;
    height: 26px;
    border: 2px solid var(--side-ink);
    transform: rotate(45deg) scale(0.8);
    display: grid;
    place-items: center;
    flex: none;
  }
  .mark::after {
    content: '';
    width: 9px;
    height: 9px;
    background: #7d9cc9;
  }
  .menu {
    display: none;
    margin-left: auto;
    color: var(--side-ink);
    font-size: 1.2rem;
  }
  nav {
    display: grid;
    gap: 2px;
  }
  nav a {
    color: var(--side-ink);
    text-decoration: none;
    padding: 0.5rem 0.7rem;
    border-radius: var(--radius-s);
    font-size: 0.95rem;
    border-left: 3px solid transparent;
  }
  nav a:hover {
    background: rgba(255, 255, 255, 0.07);
  }
  nav a[aria-current='page'] {
    background: rgba(255, 255, 255, 0.1);
    border-left-color: #7d9cc9;
    font-weight: 600;
  }
  .who {
    margin-top: auto;
    display: grid;
    gap: 0.15rem;
    padding: 0.8rem 0.7rem 0;
    border-top: 1px solid rgba(255, 255, 255, 0.12);
    font-size: 0.85rem;
  }
  .faint-side {
    color: var(--side-faint);
    font-size: 0.78rem;
  }
  .name {
    font-family: var(--mono);
    word-break: break-all;
  }
  .logout {
    justify-self: start;
    color: var(--side-ink);
    padding-inline: 0;
    margin-top: 0.3rem;
    text-decoration: underline;
    text-underline-offset: 3px;
  }
  .logout:hover:not(:disabled) {
    background: transparent;
  }
  main {
    min-width: 0;
    padding: 1.6rem clamp(1rem, 3vw, 2.4rem) 4rem;
  }
  @media (max-width: 820px) {
    .shell {
      grid-template-columns: 1fr;
    }
    aside {
      position: static;
      height: auto;
      padding: 0.8rem 1rem;
      gap: 0.8rem;
    }
    .menu {
      display: inline-flex;
    }
    nav,
    .who {
      display: none;
    }
    aside.open nav,
    aside.open .who {
      display: grid;
    }
    main {
      padding-inline: 1rem;
    }
  }
</style>
