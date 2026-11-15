<script>
  import PageHead from '../../components/PageHead.svelte';
  import Confirm from '../../components/Confirm.svelte';
  import * as s3 from '../../lib/s3.js';
  import { api } from '../../lib/api.js';
  import { link } from '../../lib/router.svelte.js';
  import { datetime, parseBytes } from '../../lib/format.js';
  import { toast } from '../../lib/toast.svelte.js';

  let { bucket, tab, info } = $props();

  const TABS = [
    ['general', '基本'],
    ['lifecycle', 'ライフサイクル'],
    ['cors', 'CORS'],
    ['uploads', '未完了のアップロード'],
  ];

  // ---- general ----
  let record = $state(null);
  let versioning = $state(undefined);
  let lock = $state(undefined);
  let lockMode = $state('');
  let lockDays = $state('');
  let lockUnit = $state('days');

  async function loadGeneral() {
    try {
      const all = await api('api/buckets');
      record = all.find((b) => b.name === bucket) ?? null;
      versioning = await s3.getVersioning(bucket);
      lock = await s3.getObjectLock(bucket);
      lockMode = lock?.mode ?? '';
      lockUnit = lock?.years ? 'years' : 'days';
      lockDays = String(lock?.years ?? lock?.days ?? '');
    } catch (e) {
      toast(s3.errorMessage(e), 'bad');
    }
  }

  async function setVersioning(status) {
    try {
      await s3.putVersioning(bucket, status);
      versioning = status;
      toast(status === 'Enabled' ? 'バージョニングを有効にしました' : 'バージョニングを停止しました');
    } catch (e) {
      toast(s3.errorMessage(e), 'bad');
    }
  }

  async function saveLock() {
    try {
      const n = Number(lockDays);
      if (lockMode && !(n > 0)) {
        toast('期間は 1 以上にしてください', 'bad');
        return;
      }
      await s3.putObjectLock(bucket, lockMode ? { mode: lockMode, [lockUnit]: n } : { mode: null });
      toast('既定の保持設定を保存しました');
      await loadGeneral();
    } catch (e) {
      toast(s3.errorMessage(e), 'bad');
    }
  }

  // ---- lifecycle ----
  let rules = $state(null);
  let lcDirty = $state(false);
  const newRule = () => ({
    id: `rule-${Date.now().toString(36)}`,
    enabled: true,
    prefix: '',
    tags: [],
    sizeGt: null,
    sizeLt: null,
    expirationDays: null,
    expirationDate: null,
    expiredObjectDeleteMarker: false,
    noncurrentDays: null,
    newerNoncurrentVersions: null,
    transitions: [],
    noncurrentTransitions: [],
    abortDays: 7,
  });

  async function loadLifecycle() {
    try {
      rules = (await s3.getLifecycle(bucket)).map((r) => ({
        ...r,
        sizeGtText: r.sizeGt == null ? '' : String(r.sizeGt),
        sizeLtText: r.sizeLt == null ? '' : String(r.sizeLt),
      }));
      lcDirty = false;
    } catch (e) {
      toast(s3.errorMessage(e), 'bad');
      rules = [];
    }
  }

  function addRule() {
    rules.push({ ...newRule(), sizeGtText: '', sizeLtText: '' });
    lcDirty = true;
  }

  async function saveLifecycle() {
    const out = [];
    for (const r of rules) {
      const gt = parseBytes(r.sizeGtText);
      const lt = parseBytes(r.sizeLtText);
      if (Number.isNaN(gt) || Number.isNaN(lt)) {
        toast(`ルール ${r.id}: サイズは「10 MiB」「500000」のように入力してください`, 'bad');
        return;
      }
      const hasAction =
        r.expirationDays || r.expirationDate || r.expiredObjectDeleteMarker || r.noncurrentDays || r.abortDays || r.transitions.length || r.noncurrentTransitions.length;
      if (!hasAction) {
        toast(`ルール ${r.id}: 何をするか（削除・移行など）を 1 つ以上指定してください`, 'bad');
        return;
      }
      out.push({ ...r, sizeGt: gt, sizeLt: lt, tags: r.tags.filter((t) => t.key.trim()) });
    }
    try {
      await s3.putLifecycle(bucket, out);
      toast(out.length ? 'ライフサイクルルールを保存しました' : 'ライフサイクルルールを削除しました');
      await loadLifecycle();
    } catch (e) {
      toast(s3.errorMessage(e), 'bad');
    }
  }

  // ---- cors ----
  let cors = $state(null);
  let corsDirty = $state(false);
  const METHODS = ['GET', 'PUT', 'POST', 'DELETE', 'HEAD'];
  const lines = (a) => (a ?? []).join('\n');
  const unlines = (s) =>
    s
      .split(/[\n,]/)
      .map((x) => x.trim())
      .filter(Boolean);

  async function loadCors() {
    try {
      cors = (await s3.getCors(bucket)).map((r) => ({
        ...r,
        originsText: lines(r.origins),
        headersText: lines(r.headers),
        exposeText: lines(r.expose),
      }));
      corsDirty = false;
    } catch (e) {
      toast(s3.errorMessage(e), 'bad');
      cors = [];
    }
  }

  function addCors() {
    cors.push({ id: '', methods: ['GET', 'HEAD'], originsText: 'https://example.com', headersText: '*', exposeText: 'ETag', maxAge: 3000 });
    corsDirty = true;
  }

  async function saveCors() {
    const out = cors.map((r) => ({
      id: r.id,
      methods: r.methods,
      origins: unlines(r.originsText),
      headers: unlines(r.headersText),
      expose: unlines(r.exposeText),
      maxAge: r.maxAge,
    }));
    if (out.some((r) => r.origins.length === 0 || r.methods.length === 0)) {
      toast('各ルールにオリジンとメソッドを 1 つ以上指定してください', 'bad');
      return;
    }
    try {
      await s3.putCors(bucket, out);
      toast(out.length ? 'CORS ルールを保存しました' : 'CORS ルールを削除しました');
      await loadCors();
    } catch (e) {
      toast(s3.errorMessage(e), 'bad');
    }
  }

  // ---- uploads ----
  let pending = $state(null);
  let abortOpen = $state(false);
  let abortTarget = $state(null);
  async function loadUploads() {
    try {
      pending = await s3.listUploads(bucket);
    } catch (e) {
      toast(s3.errorMessage(e), 'bad');
      pending = [];
    }
  }
  async function abort() {
    await s3.abortUpload(bucket, abortTarget.key, abortTarget.uploadId);
    toast('アップロードを中止しました');
    await loadUploads();
  }

  $effect(() => {
    if (tab === 'lifecycle') loadLifecycle();
    else if (tab === 'cors') loadCors();
    else if (tab === 'uploads') loadUploads();
    else loadGeneral();
  });

  const classes = $derived(info.storage_classes ?? []);
</script>

<PageHead
  title="バケットの設定"
  crumbs={[
    { label: 'バケット', href: link(['buckets']) },
    { label: bucket, href: link(['b', bucket]) },
    { label: '設定' },
  ]}
/>

<nav class="tabs" aria-label="設定の種類">
  {#each TABS as [id, label] (id)}
    <a href={link(['b', bucket, 'settings'], { tab: id })} aria-current={tab === id ? 'page' : undefined}>{label}</a>
  {/each}
</nav>

<div class="content">
  {#if tab === 'general'}
    <section class="sheet">
      <header><h2>バージョニング</h2>
        {#if versioning === 'Enabled'}<span class="badge ok">有効</span>{:else if versioning === 'Suspended'}<span class="badge warn">停止中</span>{:else if versioning !== undefined}<span class="badge">無効</span>{/if}
      </header>
      <div class="body stack">
        <p class="muted">有効にすると、上書きや削除をしても以前の版が残り、あとから取り出したり復元したりできます。残した版も保存容量に含まれます。一度有効にすると無効には戻せず、停止のみできます。</p>
        <div class="row">
          {#if versioning !== 'Enabled'}<button class="primary" onclick={() => setVersioning('Enabled')}>有効にする</button>{/if}
          {#if versioning === 'Enabled' && !lock}<button onclick={() => setVersioning('Suspended')}>停止する</button>{/if}
          {#if versioning === 'Enabled' && lock}<span class="faint small">オブジェクトロックを使うバケットは停止できません。</span>{/if}
        </div>
      </div>
    </section>

    <section class="sheet">
      <header><h2>オブジェクトロック</h2>{#if lock}<span class="badge ok">有効</span>{:else if lock === null}<span class="badge">無効</span>{/if}</header>
      <div class="body stack">
        {#if lock === null}
          <p class="muted">このバケットではオブジェクトロックを使えません（バケットの作成時にしか有効にできません）。</p>
        {:else if lock}
          <p class="muted">新しく置いたオブジェクトに自動で付ける保持期間です。保持中の版は削除できません。</p>
          <div class="form-grid">
            <label class="field"><span>既定の保持モード</span>
              <select bind:value={lockMode}>
                <option value="">なし</option>
                <option value="GOVERNANCE">GOVERNANCE（権限があれば解除可）</option>
                <option value="COMPLIANCE">COMPLIANCE（期限まで誰も削除できない）</option>
              </select>
            </label>
            {#if lockMode}
              <label class="field"><span>期間</span>
                <div class="row"><input type="number" min="1" bind:value={lockDays} style="width: 7rem" />
                  <select bind:value={lockUnit}><option value="days">日</option><option value="years">年</option></select></div>
              </label>
            {/if}
          </div>
          <div><button class="primary" onclick={saveLock}>保存</button></div>
        {/if}
      </div>
    </section>

    {#if record}
      <section class="sheet">
        <header><h2>運営側で設定する項目</h2></header>
        <div class="body">
          <dl class="props">
            <dt>作成日時</dt><dd>{datetime(record.created_at)}</dd>
            <dt>公開</dt><dd>{record.public_read ? '公開（認証なしで GET / HEAD できます）' : '非公開'}</dd>
            <dt>既定のコピー数</dt><dd>{record.replicas ?? `${info.default_replicas}（標準）`}</dd>
          </dl>
          <p class="faint small" style="margin-top: 0.8rem">公開設定と既定のコピー数を変えたい場合はサポートにお問い合わせください。オブジェクトごとのコピー数はアップロード時のストレージクラスで選べます。</p>
        </div>
      </section>
    {/if}
  {:else if tab === 'lifecycle'}
    <div class="spread">
      <p class="muted">期限を過ぎたオブジェクトの削除、古い版の削除、ストレージクラス（コピー数）の変更を自動で行います。1 時間ごとに適用されます。</p>
      <div class="row">
        <button onclick={addRule}>ルールを追加</button>
        <button class="primary" onclick={saveLifecycle} disabled={!lcDirty}>保存</button>
      </div>
    </div>
    {#if rules === null}
      <p class="faint">読み込み中…</p>
    {:else if rules.length === 0}
      <div class="sheet empty"><p>ルールはありません。「ルールを追加」から作成してください。</p></div>
    {:else}
      {#each rules as r, i (i)}
        <section class="sheet" oninput={() => (lcDirty = true)} onchange={() => (lcDirty = true)}>
          <header>
            <div class="row">
              <input type="text" bind:value={r.id} aria-label="ルール名" placeholder="ルール名" />
              <label class="check small"><input type="checkbox" bind:checked={r.enabled} /> 有効</label>
            </div>
            <button class="danger small" onclick={() => { rules.splice(i, 1); lcDirty = true; }}>ルールを削除</button>
          </header>
          <div class="body stack">
            <h3>対象</h3>
            <div class="form-grid">
              <label class="field"><span>プレフィックス</span><input type="text" bind:value={r.prefix} placeholder="空ならバケット全体" /></label>
              <label class="field"><span>サイズがこれより大きい</span><input type="text" bind:value={r.sizeGtText} placeholder="例: 1 MiB" /></label>
              <label class="field"><span>サイズがこれより小さい</span><input type="text" bind:value={r.sizeLtText} placeholder="例: 1 GiB" /></label>
            </div>
            <div class="stack small">
              {#each r.tags as t, j (j)}
                <div class="row">
                  <input type="text" bind:value={t.key} placeholder="タグのキー" aria-label="タグのキー" />
                  <input type="text" bind:value={t.value} placeholder="値" aria-label="タグの値" />
                  <button class="quiet small" onclick={() => { r.tags.splice(j, 1); lcDirty = true; }} aria-label="タグ条件を削除">×</button>
                </div>
              {/each}
              <div><button class="small" onclick={() => { r.tags.push({ key: '', value: '' }); lcDirty = true; }}>タグ条件を追加</button>
                <span class="faint">（指定したタグをすべて持つオブジェクトだけが対象）</span></div>
            </div>

            <h3>処理</h3>
            <div class="form-grid">
              <label class="field"><span>現行版を削除するまでの日数</span><input type="number" min="1" bind:value={r.expirationDays} disabled={!!r.expirationDate} /></label>
              <label class="field"><span>または、この日以降に削除</span><input type="date" bind:value={r.expirationDate} disabled={r.expirationDays != null && r.expirationDays !== ''} /></label>
              <label class="field"><span>古い版を削除するまでの日数</span><input type="number" min="1" bind:value={r.noncurrentDays} /><span class="hint">古い版になってからの日数</span></label>
              <label class="field"><span>残す古い版の数</span><input type="number" min="1" bind:value={r.newerNoncurrentVersions} disabled={!r.noncurrentDays} /><span class="hint">新しいものから N 個は日数を過ぎても残す</span></label>
              <label class="field"><span>未完了アップロードを中止するまでの日数</span><input type="number" min="1" bind:value={r.abortDays} /></label>
            </div>
            <label class="check small"><input type="checkbox" bind:checked={r.expiredObjectDeleteMarker} disabled={r.tags.length > 0 || !!r.expirationDays || !!r.expirationDate} /> 他の版が残っていない削除マーカーを消す</label>

            {#if classes.length}
              <h3>ストレージクラスの変更</h3>
              {#each r.transitions as t, j (j)}
                <div class="row small">
                  作成から <input type="number" min="0" bind:value={t.days} style="width: 6rem" aria-label="日数" /> 日後に
                  <select bind:value={t.storageClass} aria-label="移行先">{#each classes as c (c.name)}<option value={c.name}>{c.name}（コピー {c.replicas}）</option>{/each}</select>
                  へ <button class="quiet small" onclick={() => { r.transitions.splice(j, 1); lcDirty = true; }} aria-label="移行を削除">×</button>
                </div>
              {/each}
              {#each r.noncurrentTransitions as t, j (j)}
                <div class="row small">
                  古い版になってから <input type="number" min="0" bind:value={t.days} style="width: 6rem" aria-label="日数" /> 日後に
                  <select bind:value={t.storageClass} aria-label="移行先">{#each classes as c (c.name)}<option value={c.name}>{c.name}（コピー {c.replicas}）</option>{/each}</select>
                  へ <button class="quiet small" onclick={() => { r.noncurrentTransitions.splice(j, 1); lcDirty = true; }} aria-label="移行を削除">×</button>
                </div>
              {/each}
              <div class="row">
                <button class="small" onclick={() => { r.transitions.push({ days: 30, storageClass: classes[0].name }); lcDirty = true; }}>現行版の移行を追加</button>
                <button class="small" onclick={() => { r.noncurrentTransitions.push({ days: 30, storageClass: classes[0].name }); lcDirty = true; }}>古い版の移行を追加</button>
              </div>
              <p class="faint small">ストレージクラスはコピー数の違いです。移行が複数あるときは日数が最も大きいものが適用されます。</p>
            {/if}
          </div>
        </section>
      {/each}
    {/if}
  {:else if tab === 'cors'}
    <div class="spread">
      <p class="muted">ブラウザの JavaScript から別のサイト経由でこのバケットにアクセスさせる場合に設定します。</p>
      <div class="row">
        <button onclick={addCors}>ルールを追加</button>
        <button class="primary" onclick={saveCors} disabled={!corsDirty}>保存</button>
      </div>
    </div>
    {#if cors === null}
      <p class="faint">読み込み中…</p>
    {:else if cors.length === 0}
      <div class="sheet empty"><p>CORS ルールはありません。</p></div>
    {:else}
      {#each cors as r, i (i)}
        <section class="sheet" oninput={() => (corsDirty = true)} onchange={() => (corsDirty = true)}>
          <header>
            <input type="text" bind:value={r.id} placeholder="ルール名（任意）" aria-label="ルール名" />
            <button class="danger small" onclick={() => { cors.splice(i, 1); corsDirty = true; }}>ルールを削除</button>
          </header>
          <div class="body form-grid">
            <label class="field"><span>許可するオリジン（1 行に 1 つ）</span><textarea rows="3" bind:value={r.originsText}></textarea></label>
            <fieldset class="field">
              <legend>許可するメソッド</legend>
              {#each METHODS as m (m)}
                <label class="check small"><input type="checkbox" checked={r.methods.includes(m)} onchange={(e) => { r.methods = e.target.checked ? [...r.methods, m] : r.methods.filter((x) => x !== m); corsDirty = true; }} /> {m}</label>
              {/each}
            </fieldset>
            <label class="field"><span>許可するリクエストヘッダ</span><textarea rows="3" bind:value={r.headersText}></textarea></label>
            <label class="field"><span>ブラウザに見せるレスポンスヘッダ</span><textarea rows="3" bind:value={r.exposeText}></textarea></label>
            <label class="field"><span>プリフライトのキャッシュ秒数</span><input type="number" min="0" bind:value={r.maxAge} /></label>
          </div>
        </section>
      {/each}
    {/if}
  {:else if tab === 'uploads'}
    <p class="muted">途中で止まったマルチパートアップロードです。送信済みの部分は容量を使います。ライフサイクルでも自動で中止できます。</p>
    <section class="sheet">
      {#if pending === null}
        <div class="body faint">読み込み中…</div>
      {:else if pending.length === 0}
        <div class="empty"><p>未完了のアップロードはありません。</p></div>
      {:else}
        <div class="table-wrap">
          <table class="grid">
            <thead><tr><th>キー</th><th>開始日時</th><th>ストレージクラス</th><th></th></tr></thead>
            <tbody>
              {#each pending as u (u.uploadId)}
                <tr>
                  <td class="break">{u.key}</td>
                  <td class="muted">{datetime(u.initiated)}</td>
                  <td class="small muted">{u.storageClass ?? 'STANDARD'}</td>
                  <td class="actions"><button class="danger small" onclick={() => { abortTarget = u; abortOpen = true; }}>中止</button></td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </section>
  {/if}
</div>

<Confirm bind:open={abortOpen} title="アップロードを中止" action="中止" message={abortTarget ? `${abortTarget.key} のアップロードを中止し、送信済みの部分を削除します。` : ''} onconfirm={abort} />

<style>
  .content {
    display: grid;
    gap: 1.2rem;
    margin-top: 1.2rem;
  }
  fieldset {
    border: none;
    padding: 0;
    margin: 0;
    display: grid;
    gap: 0.3rem;
  }
  legend {
    padding: 0;
    margin-bottom: 0.25rem;
  }
</style>
