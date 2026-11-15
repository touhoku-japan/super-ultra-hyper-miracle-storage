<script>
  // Asks the server which console this is (customer at /, operator at /ui/) and whether the
  // session is valid, then shows the login screen or that console.
  import { session, setSessionExpiredHandler } from './lib/api.js';
  import Login from './pages/Login.svelte';
  import CustomerApp from './CustomerApp.svelte';
  import OperatorApp from './OperatorApp.svelte';
  import Toasts from './components/Toasts.svelte';
  import { toast } from './lib/toast.svelte.js';

  let info = $state(null);
  let error = $state(null);

  async function load() {
    try {
      info = await session.get();
      error = null;
    } catch (e) {
      error = e.message;
    }
  }

  setSessionExpiredHandler(() => {
    if (info?.logged_in) {
      info = { ...info, logged_in: false };
      toast('セッションが切れました。もう一度ログインしてください', 'bad');
    }
  });

  async function logout() {
    await session.logout().catch(() => {});
    await load();
  }

  load();
</script>

{#if error}
  <main class="fatal">
    <p class="notice bad">コンソールを読み込めませんでした: {error}</p>
    <button onclick={load}>再読み込み</button>
  </main>
{:else if !info}
  <main class="fatal"><p class="faint">読み込み中…</p></main>
{:else if !info.logged_in}
  <Login mode={info.mode} onlogin={load} />
{:else if info.mode === 'operator'}
  <OperatorApp {info} onlogout={logout} />
{:else}
  <CustomerApp {info} onlogout={logout} />
{/if}
<Toasts />

<style>
  .fatal {
    min-height: 100vh;
    display: grid;
    place-content: center;
    gap: 1rem;
    justify-items: center;
    padding: 1rem;
  }
</style>
