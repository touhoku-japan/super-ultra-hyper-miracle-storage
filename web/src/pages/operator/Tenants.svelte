<script>
  import PageHead from '../../components/PageHead.svelte';
  import Modal from '../../components/Modal.svelte';
  import { admin } from '../../lib/admin.js';
  import { link, go } from '../../lib/router.svelte.js';
  import { bytes, count, datetime, parseBytes } from '../../lib/format.js';
  import { toast } from '../../lib/toast.svelte.js';

  let tenants = $state(null);
  let usage = $state({});
  let error = $state(null);
  let filter = $state('');

  async function load() {
    try {
      const [t, s] = await Promise.all([admin.tenants(), admin.storage()]);
      tenants = t;
      usage = Object.fromEntries(s.map((u) => [u.tenant_id, u]));
    } catch (e) {
      error = e.message;
    }
  }
  load();

  let open = $state(false);
  let name = $state('');
  let quota = $state('');
  let maxBuckets = $state('');
  let busy = $state(false);
  let formError = $state(null);

  async function create(e) {
    e.preventDefault();
    const q = parseBytes(quota);
    if (Number.isNaN(q)) {
      formError = '容量は「100 GiB」「1T」のように入力してください';
      return;
    }
    busy = true;
    formError = null;
    try {
      const t = await admin.createTenant({
        name: name.trim(),
        quota_bytes: q,
        max_buckets: maxBuckets === '' ? null : Number(maxBuckets),
      });
      toast(`テナント ${t.name} を作成しました`);
      open = false;
      go(['tenants', t.id]);
    } catch (err) {
      formError = err.message;
    } finally {
      busy = false;
    }
  }

  const shown = $derived((tenants ?? []).filter((t) => t.name.includes(filter.trim()) || t.id.includes(filter.trim())));
</script>

<PageHead title="テナント">
  <button class="primary" onclick={() => (open = true)}>テナントを作成</button>
</PageHead>

{#if error}
  <p class="notice bad">{error}</p>
{:else if !tenants}
  <p class="faint">読み込み中…</p>
{:else}
  <section class="sheet">
    <header>
      <input type="search" placeholder="名前か ID で絞り込み" bind:value={filter} aria-label="テナントを絞り込み" />
      <span class="faint small">{tenants.length} 件</span>
    </header>
    {#if tenants.length === 0}
      <div class="empty"><p>テナントはまだありません。通常は SoraHost ポータルが申込時に作成します。</p></div>
    {:else}
      <div class="table-wrap">
        <table class="grid">
          <thead><tr><th>名前</th><th>状態</th><th class="num">保存量</th><th class="num">容量上限</th><th class="num">オブジェクト</th><th>作成日時</th></tr></thead>
          <tbody>
            {#each shown as t (t.id)}
              {@const u = usage[t.id]}
              <tr>
                <td><a href={link(['tenants', t.id])}><strong>{t.name}</strong></a><br /><span class="mono faint small">{t.id}</span></td>
                <td>{#if t.suspended}<span class="badge bad">停止中</span>{:else}<span class="badge ok">有効</span>{/if}</td>
                <td class="num">{bytes(u?.logical_bytes ?? 0)}</td>
                <td class="num muted">{t.quota_bytes != null ? bytes(t.quota_bytes) : 'なし'}</td>
                <td class="num">{count(u?.objects ?? 0)}</td>
                <td class="muted">{datetime(t.created_at)}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </section>
{/if}

<Modal bind:open title="テナントを作成">
  <form id="new-tenant" class="stack" onsubmit={create}>
    <label class="field"><span>名前</span><input type="text" bind:value={name} required autocomplete="off" /><span class="hint">ポータルの契約 ID など、一意の名前</span></label>
    <div class="form-grid">
      <label class="field"><span>容量上限</span><input type="text" bind:value={quota} placeholder="なし" /><span class="hint">例: 100 GiB（論理サイズ）</span></label>
      <label class="field"><span>バケット数の上限</span><input type="number" min="0" bind:value={maxBuckets} placeholder="なし" /></label>
    </div>
    {#if formError}<p class="notice bad">{formError}</p>{/if}
  </form>
  {#snippet footer()}
    <button onclick={() => (open = false)}>キャンセル</button>
    <button class="primary" type="submit" form="new-tenant" disabled={busy || !name.trim()}>{busy ? '作成中…' : '作成'}</button>
  {/snippet}
</Modal>
