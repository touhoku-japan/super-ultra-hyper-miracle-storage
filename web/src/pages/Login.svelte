<script>
  import { session } from '../lib/api.js';

  let { mode, onlogin } = $props();
  let accessKey = $state('');
  let secret = $state('');
  let token = $state('');
  let error = $state(null);
  let busy = $state(false);

  async function submit(e) {
    e.preventDefault();
    busy = true;
    error = null;
    try {
      await session.login(
        mode === 'operator' ? { token } : { access_key_id: accessKey.trim(), secret_access_key: secret },
      );
      secret = '';
      token = '';
      onlogin();
    } catch (err) {
      error = err.message;
    } finally {
      busy = false;
    }
  }
</script>

<div class="login">
  <section class="wall lattice" aria-hidden="true">
    <div class="sign">
      <span>SUHMS</span>
      <span class="sub">{mode === 'operator' ? '運用コンソール' : 'オブジェクトストレージ'}</span>
    </div>
  </section>
  <section class="form-side">
    <form onsubmit={submit}>
      <h1>{mode === 'operator' ? '運用コンソールにログイン' : 'コンソールにログイン'}</h1>
      {#if mode === 'operator'}
        <p class="muted small">管理 API のトークン（<code>[admin] token_file</code> または <code>SUHMS_ADMIN_TOKEN</code>）を入力してください。</p>
        <label class="field">
          <span>管理トークン</span>
          <input type="password" bind:value={token} autocomplete="current-password" required />
        </label>
      {:else}
        <p class="muted small">S3 クライアントと同じアクセスキーでログインします。シークレットキーはこのブラウザには保存されません。</p>
        <label class="field">
          <span>アクセスキー ID</span>
          <input type="text" bind:value={accessKey} autocomplete="username" spellcheck="false" required />
        </label>
        <label class="field">
          <span>シークレットアクセスキー</span>
          <input type="password" bind:value={secret} autocomplete="current-password" required />
        </label>
      {/if}
      {#if error}<p class="notice bad" role="alert">{error}</p>{/if}
      <button class="primary" type="submit" disabled={busy}>{busy ? 'ログイン中…' : 'ログイン'}</button>
    </form>
  </section>
</div>

<style>
  .login {
    min-height: 100vh;
    display: grid;
    grid-template-columns: minmax(0, 5fr) minmax(0, 6fr);
  }
  .wall {
    display: grid;
    align-content: end;
    padding: 2.5rem;
    color: var(--side-ink);
  }
  .sign {
    display: grid;
    gap: 0.4rem;
    border-left: 3px solid #7d9cc9;
    padding-left: 1rem;
  }
  .sign span:first-child {
    font-size: clamp(2.2rem, 5vw, 3.6rem);
    font-weight: 700;
    letter-spacing: 0.28em;
    line-height: 1;
  }
  .sub {
    color: var(--side-faint);
    letter-spacing: 0.1em;
  }
  .form-side {
    display: grid;
    place-items: center;
    padding: 2rem 1rem;
  }
  form {
    width: min(380px, 100%);
    display: grid;
    gap: 1rem;
  }
  form button {
    justify-self: start;
    padding-inline: 1.6rem;
  }
  @media (max-width: 760px) {
    .login {
      grid-template-columns: 1fr;
      grid-template-rows: 150px 1fr;
    }
    .wall {
      padding: 1.4rem;
    }
  }
</style>
