<script>
  import PageHead from '../../components/PageHead.svelte';
  import Modal from '../../components/Modal.svelte';
  import Confirm from '../../components/Confirm.svelte';
  import { admin } from '../../lib/admin.js';
  import { link } from '../../lib/router.svelte.js';
  import { bytes, count, datetime, parseBytes } from '../../lib/format.js';
  import { toast } from '../../lib/toast.svelte.js';

  let { id, info } = $props();

  let t = $state(null);
  let keys = $state([]);
  let buckets = $state([]);
  let usage = $state(null);
  let error = $state(null);

  let quota = $state('');
  let maxBuckets = $state('');

  function monthRange(offset) {
    const now = new Date();
    const from = Date.UTC(now.getUTCFullYear(), now.getUTCMonth() + offset, 1);
    const to = Date.UTC(now.getUTCFullYear(), now.getUTCMonth() + offset + 1, 1);
    return { from, to };
  }
  let month = $state(0);

  async function load() {
    try {
      t = await admin.tenant(id);
      quota = t.quota_bytes != null ? bytes(t.quota_bytes) : '';
      maxBuckets = t.max_buckets != null ? String(t.max_buckets) : '';
      [keys, buckets] = await Promise.all([admin.keys(id), admin.buckets(id)]);
      error = null;
    } catch (e) {
      error = e.message;
    }
  }
  async function loadUsage() {
    const { from, to } = monthRange(month);
    try {
      usage = await admin.usage(id, from, to);
    } catch (e) {
      toast(e.message, 'bad');
    }
  }
  load();
  $effect(() => {
    void month;
    loadUsage();
  });

  async function saveLimits(e) {
    e.preventDefault();
    const q = parseBytes(quota);
    if (Number.isNaN(q)) {
      toast('容量は「100 GiB」のように入力してください', 'bad');
      return;
    }
    try {
      await admin.patchTenant(id, { quota_bytes: q, max_buckets: maxBuckets === '' ? null : Number(maxBuckets) });
      toast('上限を保存しました');
      await load();
    } catch (err) {
      toast(err.message, 'bad');
    }
  }

  let suspendOpen = $state(false);
  async function setSuspended(on) {
    await admin.patchTenant(id, { suspended: on });
    toast(on ? 'テナントを停止しました' : 'テナントの停止を解除しました');
    await load();
  }

  let newKey = $state(null);
  async function createKey() {
    try {
      newKey = await admin.createKey(id);
      await load();
    } catch (e) {
      toast(e.message, 'bad');
    }
  }
  async function copy(text) {
    try {
      await navigator.clipboard.writeText(text);
      toast('コピーしました');
    } catch {
      toast('コピーできませんでした。選択してコピーしてください', 'bad');
    }
  }
  let keyTarget = $state(null);
  let keyOpen = $state(false);
  async function toggleKey() {
    await admin.patchKey(keyTarget.access_key_id, { active: !keyTarget.active });
    toast(keyTarget.active ? 'アクセスキーを無効にしました' : 'アクセスキーを有効にしました');
    await load();
  }

  let bucketEdit = $state(null);
  let bucketReplicas = $state('');
  let bucketPublic = $state(false);
  let bucketOpen = $state(false);
  function editBucket(b) {
    bucketEdit = b;
    bucketReplicas = b.replicas == null ? '' : String(b.replicas);
    bucketPublic = b.public_read;
    bucketOpen = true;
  }
  async function saveBucket() {
    try {
      const body = { replicas: bucketReplicas === '' ? null : Number(bucketReplicas) };
      if (bucketPublic !== bucketEdit.public_read) body.public_read = bucketPublic;
      await admin.patchBucket(bucketEdit.name, body);
      toast(`${bucketEdit.name} の設定を保存しました`);
      bucketOpen = false;
      await load();
    } catch (e) {
      toast(e.message, 'bad');
    }
  }

  const monthLabel = $derived.by(() => {
    const { from } = monthRange(month);
    const d = new Date(from);
    return `${d.getUTCFullYear()} 年 ${d.getUTCMonth() + 1} 月`;
  });
  const hoursInMonth = $derived.by(() => {
    const { from, to } = monthRange(month);
    return (to - from) / 3600000;
  });
</script>

<PageHead title={t?.name ?? 'テナント'} crumbs={[{ label: 'テナント', href: link(['tenants']) }, { label: t?.name ?? id }]}>
  {#if t}
    {#if t.suspended}
      <button class="primary" onclick={() => setSuspended(false).catch((e) => toast(e.message, 'bad'))}>停止を解除</button>
    {:else}
      <button class="danger" onclick={() => (suspendOpen = true)}>テナントを停止</button>
    {/if}
  {/if}
</PageHead>

{#if error}
  <p class="notice bad">{error}</p>
{:else if !t}
  <p class="faint">読み込み中…</p>
{:else}
  {#if t.suspended}<p class="notice bad">このテナントは停止中です。すべての S3 リクエストとコンソールへのログインを拒否しています。</p>{/if}
  <div class="cols">
    <section class="sheet">
      <header><h2>概要</h2></header>
      <div class="body stack">
        <dl class="props">
          <dt>ID</dt><dd class="mono">{t.id}</dd>
          <dt>作成日時</dt><dd>{datetime(t.created_at)}</dd>
          <dt>保存量</dt><dd>{bytes(t.logical_bytes)}（ディスク上 {bytes(t.stored_bytes)}）</dd>
          <dt>オブジェクト</dt><dd>{count(t.objects)}</dd>
        </dl>
        <form class="stack" onsubmit={saveLimits}>
          <div class="form-grid">
            <label class="field"><span>容量上限</span><input type="text" bind:value={quota} placeholder="なし" /></label>
            <label class="field"><span>バケット数の上限</span><input type="number" min="0" bind:value={maxBuckets} placeholder="なし" /></label>
          </div>
          <div><button class="primary" type="submit">上限を保存</button></div>
        </form>
      </div>
    </section>

    <section class="sheet">
      <header>
        <h2>利用量</h2>
        <div class="row">
          <button class="small" onclick={() => (month -= 1)} aria-label="前の月">‹</button>
          <span class="small">{monthLabel}</span>
          <button class="small" onclick={() => (month += 1)} disabled={month >= 0} aria-label="次の月">›</button>
        </div>
      </header>
      <div class="body">
        {#if usage}
          <table class="grid">
            <tbody>
              <tr><td>平均保存量</td><td class="num">{bytes(usage.logical_byte_hours / hoursInMonth)}</td></tr>
              <tr><td>クラス A リクエスト</td><td class="num">{count(usage.class_a_requests)}</td></tr>
              <tr><td>クラス B リクエスト</td><td class="num">{count(usage.class_b_requests)}</td></tr>
              <tr><td>アップロード量</td><td class="num">{bytes(usage.ingress_bytes)}</td></tr>
              <tr><td>ダウンロード量</td><td class="num">{bytes(usage.egress_bytes)}</td></tr>
            </tbody>
          </table>
          <p class="faint small" style="margin-top: 0.6rem">平均保存量は 1 時間ごとの記録の合計 ÷ その月の時間数。課金計算は docs/sorahost-integration.md を参照。</p>
        {:else}
          <p class="faint">読み込み中…</p>
        {/if}
      </div>
    </section>
  </div>

  <section class="sheet">
    <header><h2>アクセスキー</h2><button onclick={createKey}>アクセスキーを発行</button></header>
    {#if keys.length === 0}
      <div class="empty"><p>アクセスキーはありません。</p></div>
    {:else}
      <div class="table-wrap">
        <table class="grid">
          <thead><tr><th>アクセスキー ID</th><th>状態</th><th>発行日時</th><th></th></tr></thead>
          <tbody>
            {#each keys as k (k.access_key_id)}
              <tr>
                <td class="mono">{k.access_key_id}</td>
                <td>{#if k.active}<span class="badge ok">有効</span>{:else}<span class="badge">無効</span>{/if}</td>
                <td class="muted">{datetime(k.created_at)}</td>
                <td class="actions">
                  <button class={k.active ? 'danger small' : 'small'} onclick={() => { keyTarget = k; keyOpen = true; }}>{k.active ? '無効にする' : '有効にする'}</button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </section>

  <section class="sheet">
    <header><h2>バケット</h2><span class="faint small">{buckets.length} 個</span></header>
    {#if buckets.length === 0}
      <div class="empty"><p>バケットはありません。</p></div>
    {:else}
      <div class="table-wrap">
        <table class="grid">
          <thead><tr><th>名前</th><th>既定のコピー数</th><th>公開</th><th>作成日時</th><th></th></tr></thead>
          <tbody>
            {#each buckets as b (b.name)}
              <tr>
                <td><strong>{b.name}</strong></td>
                <td>{b.replicas ?? 'クラスタの既定'}</td>
                <td>{#if b.public_read}<span class="badge warn">公開</span>{:else}<span class="faint small">非公開</span>{/if}</td>
                <td class="muted">{datetime(b.created_at)}</td>
                <td class="actions"><button class="small" onclick={() => editBucket(b)}>変更</button></td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </section>
{/if}

<Confirm bind:open={suspendOpen} title="テナントを停止" action="停止" confirmText={t?.name} onconfirm={() => setSuspended(true)}>
  <p>停止すると、このテナントのすべての S3 リクエストとコンソールへのログインを拒否します（他ノードでは最大 10 秒遅れて反映）。データは消えません。</p>
</Confirm>

<Confirm bind:open={keyOpen} title={keyTarget?.active ? 'アクセスキーを無効にする' : 'アクセスキーを有効にする'} action={keyTarget?.active ? '無効にする' : '有効にする'} danger={keyTarget?.active} onconfirm={toggleKey}>
  {#if keyTarget?.active}
    <p><code>{keyTarget.access_key_id}</code> を使うクライアント・コンソールのセッション・共有リンクが使えなくなります。</p>
  {:else}
    <p><code>{keyTarget?.access_key_id}</code> をもう一度使えるようにします。</p>
  {/if}
</Confirm>

<Modal open={!!newKey} title="アクセスキーを発行しました" onclose={() => (newKey = null)}>
  {#if newKey}
    <p class="notice warn">シークレットアクセスキーは今しか表示されません。顧客に安全な方法で渡してください。</p>
    <dl class="props">
      <dt>アクセスキー ID</dt><dd class="mono">{newKey.access_key_id} <button class="quiet small" onclick={() => copy(newKey.access_key_id)}>コピー</button></dd>
      <dt>シークレット</dt><dd class="mono">{newKey.secret_access_key} <button class="quiet small" onclick={() => copy(newKey.secret_access_key)}>コピー</button></dd>
    </dl>
  {/if}
  {#snippet footer()}<button class="primary" onclick={() => (newKey = null)}>控えました</button>{/snippet}
</Modal>

<Modal bind:open={bucketOpen} title={bucketEdit ? `${bucketEdit.name} の設定` : ''}>
  <label class="field">
    <span>既定のコピー数</span>
    <select bind:value={bucketReplicas}>
      <option value="">クラスタの既定</option>
      {#each [1, 2, 3, 4, 5] as n (n)}<option value={String(n)}>{n}</option>{/each}
    </select>
    <span class="hint">ストレージクラスを指定しないオブジェクトのコピー数。変更すると既存のオブジェクトも repair が増減します。1 はディスク 1 本の故障でデータを失います。</span>
  </label>
  <label class="check">
    <input type="checkbox" bind:checked={bucketPublic} disabled={!info.allow_public_buckets && !bucketPublic} />
    公開（認証なしで GET / HEAD を許可。通信量は所有者に課金）
  </label>
  {#if !info.allow_public_buckets}<p class="faint small">このサーバでは公開バケットが無効です（[s3] allow_public_buckets）。</p>{/if}
  {#snippet footer()}
    <button onclick={() => (bucketOpen = false)}>キャンセル</button>
    <button class="primary" onclick={saveBucket}>保存</button>
  {/snippet}
</Modal>

<style>
  .cols {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(340px, 1fr));
    gap: 1.2rem;
    margin-bottom: 1.2rem;
    align-items: start;
  }
  /* Cards below the two-column row; the row itself spaces its cards with gap. */
  .cols ~ section.sheet + section.sheet {
    margin-top: 1.2rem;
  }
</style>
