<script>
  // Objects of a bucket, one "folder" (prefix up to the next `/`) at a time.
  import PageHead from '../../components/PageHead.svelte';
  import Modal from '../../components/Modal.svelte';
  import Confirm from '../../components/Confirm.svelte';
  import ObjectPanel from './ObjectPanel.svelte';
  import * as s3 from '../../lib/s3.js';
  import { link, go } from '../../lib/router.svelte.js';
  import { bytes, datetime, basename, count } from '../../lib/format.js';
  import { toast } from '../../lib/toast.svelte.js';

  let { bucket, prefix, versions, info } = $props();

  let folders = $state([]);
  let objects = $state([]);
  let token = $state(null);
  let truncated = $state(false);
  let loading = $state(false);
  let error = $state(null);
  let filter = $state('');
  let selected = $state(new Set());
  let lock = $state(null);
  let versioning = $state(null);
  let detail = $state(null); // { key, versionId }

  // ---- listing ----

  let seq = 0;
  async function load(more = false) {
    const mine = ++seq;
    loading = true;
    error = null;
    try {
      if (versions) {
        const page = await s3.listVersions(bucket, {
          prefix,
          delimiter: '/',
          keyMarker: more ? token?.key : undefined,
          versionIdMarker: more ? token?.version : undefined,
        });
        if (mine !== seq) return;
        folders = more ? [...folders, ...page.prefixes] : page.prefixes;
        objects = more ? [...objects, ...page.entries] : page.entries;
        truncated = page.truncated;
        token = { key: page.nextKeyMarker, version: page.nextVersionIdMarker };
      } else {
        const page = await s3.listObjects(bucket, { prefix, token: more ? token : undefined });
        if (mine !== seq) return;
        folders = more ? [...folders, ...page.prefixes] : page.prefixes;
        // The folder's own placeholder object ("prefix/") is not listed as a file.
        const files = page.objects.filter((o) => o.key !== prefix);
        objects = more ? [...objects, ...files] : files;
        truncated = page.truncated;
        token = page.nextToken;
      }
      if (!more) selected = new Set();
    } catch (e) {
      if (mine === seq) error = s3.errorMessage(e);
    } finally {
      if (mine === seq) loading = false;
    }
  }

  $effect(() => {
    // Reload whenever the folder or the versions toggle changes.
    void prefix;
    void versions;
    filter = '';
    load();
  });

  // The page is re-created per bucket ({#key} in CustomerApp), so this runs once per bucket.
  function loadBucketSettings() {
    s3.getObjectLock(bucket).then((l) => (lock = l)).catch(() => (lock = null));
    s3.getVersioning(bucket).then((v) => (versioning = v)).catch(() => {});
  }
  loadBucketSettings();

  const crumbs = $derived.by(() => {
    const parts = prefix.split('/').filter(Boolean);
    const out = [{ label: 'バケット', href: link(['buckets']) }, { label: bucket, href: link(['b', bucket]) }];
    let acc = '';
    for (const p of parts.slice(0, -1)) {
      acc += `${p}/`;
      out.push({ label: p, href: link(['b', bucket], { prefix: acc }) });
    }
    return out;
  });
  const title = $derived(prefix ? basename(prefix).replace(/\/$/, '') : bucket);

  const f = $derived(filter.trim().toLowerCase());
  const shownFolders = $derived(folders.filter((p) => basename(p).toLowerCase().includes(f)));
  const shownObjects = $derived(objects.filter((o) => basename(o.key).toLowerCase().includes(f)));

  const rowId = (o) => (versions ? `${o.key}\u0000${o.versionId}` : o.key);
  function toggle(id) {
    const s = new Set(selected);
    if (s.has(id)) s.delete(id);
    else s.add(id);
    selected = s;
  }
  const allSelected = $derived(shownObjects.length > 0 && shownObjects.every((o) => selected.has(rowId(o))));
  function toggleAll() {
    selected = allSelected ? new Set() : new Set(shownObjects.map(rowId));
  }

  function setVersions(on) {
    go(['b', bucket], { prefix, versions: on ? '1' : undefined });
  }

  // ---- uploads ----

  let uploads = $state([]); // { id, name, key, size, sent, status, error, ctrl }
  let nextUpload = 1;
  let storageClass = $state('');
  let dragging = $state(false);
  let fileInput = $state();
  let dirInput = $state();

  function enqueue(files) {
    for (const { file, path } of files) {
      uploads.push({
        id: nextUpload++,
        name: path,
        key: prefix + path,
        size: file.size,
        sent: 0,
        status: 'waiting',
        error: null,
        file,
        ctrl: new AbortController(),
      });
    }
    pump();
  }

  let active = 0;
  const MAX_ACTIVE = 2;
  function pump() {
    while (active < MAX_ACTIVE) {
      const u = uploads.find((x) => x.status === 'waiting');
      if (!u) break;
      active += 1;
      u.status = 'sending';
      s3.upload(bucket, u.key, u.file, {
        storageClass: storageClass || undefined,
        signal: u.ctrl.signal,
        onProgress: (n) => (u.sent = n),
      })
        .then(() => {
          u.status = 'done';
          u.sent = u.size;
        })
        .catch((e) => {
          u.status = e?.name === 'AbortError' ? 'cancelled' : 'failed';
          u.error = s3.errorMessage(e);
        })
        .finally(() => {
          active -= 1;
          u.file = null;
          pump();
          if (!uploads.some((x) => x.status === 'waiting' || x.status === 'sending')) {
            const done = uploads.filter((x) => x.status === 'done').length;
            if (done) toast(`${done} 件のアップロードが完了しました`);
            load();
          }
        });
    }
  }

  function cancel(u) {
    if (u.status === 'waiting') u.status = 'cancelled';
    else u.ctrl.abort();
  }
  function clearFinished() {
    uploads = uploads.filter((u) => u.status === 'waiting' || u.status === 'sending');
  }
  const uploading = $derived(uploads.some((u) => u.status === 'waiting' || u.status === 'sending'));

  function onPick(e, dirs) {
    const list = Array.from(e.target.files ?? []).map((file) => ({
      file,
      path: dirs && file.webkitRelativePath ? file.webkitRelativePath : file.name,
    }));
    e.target.value = '';
    enqueue(list);
  }

  /** Files of a drop, walking dropped folders. */
  async function droppedFiles(dt) {
    const out = [];
    const walk = (entry, path) =>
      new Promise((resolve) => {
        if (entry.isFile) {
          entry.file(
            (file) => {
              out.push({ file, path: path + file.name });
              resolve();
            },
            () => resolve(),
          );
        } else if (entry.isDirectory) {
          const reader = entry.createReader();
          const all = [];
          const readBatch = () =>
            reader.readEntries(async (batch) => {
              if (batch.length === 0) {
                for (const e of all) await walk(e, `${path}${entry.name}/`);
                resolve();
              } else {
                all.push(...batch);
                readBatch();
              }
            }, () => resolve());
          readBatch();
        } else resolve();
      });
    const entries = Array.from(dt.items ?? [])
      .map((i) => i.webkitGetAsEntry?.())
      .filter(Boolean);
    if (entries.length === 0) return Array.from(dt.files).map((file) => ({ file, path: file.name }));
    for (const e of entries) await walk(e, '');
    return out;
  }

  async function onDrop(e) {
    e.preventDefault();
    dragging = false;
    enqueue(await droppedFiles(e.dataTransfer));
  }

  // Leaving the page with uploads running would cut them off.
  $effect(() => {
    if (!uploading) return;
    const warn = (e) => {
      e.preventDefault();
      e.returnValue = '';
    };
    window.addEventListener('beforeunload', warn);
    return () => window.removeEventListener('beforeunload', warn);
  });

  // ---- folders ----

  let folderOpen = $state(false);
  let folderName = $state('');
  async function createFolder(e) {
    e.preventDefault();
    const n = folderName.trim().replace(/^\/+|\/+$/g, '');
    if (!n) return;
    try {
      await s3.createFolder(bucket, prefix + n + '/');
      folderOpen = false;
      folderName = '';
      go(['b', bucket], { prefix: prefix + n + '/' });
    } catch (err) {
      toast(s3.errorMessage(err), 'bad');
    }
  }

  // ---- deletion ----

  let deleteOpen = $state(false);
  let deleteTargets = $state({ objects: [], folders: [] });
  let bypass = $state(false);
  let deleteProgress = $state(null);

  function askDelete(objs, flds = []) {
    deleteTargets = { objects: objs, folders: flds };
    bypass = false;
    deleteOpen = true;
  }
  function askDeleteSelected() {
    askDelete(objects.filter((o) => selected.has(rowId(o))));
  }

  async function runDelete() {
    const list = deleteTargets.objects.map((o) => ({ key: o.key, versionId: versions ? o.versionId : undefined }));
    for (const p of deleteTargets.folders) {
      deleteProgress = `${p} の中身を確認中…`;
      for await (const o of s3.allKeys(bucket, p)) list.push({ key: o.key });
      list.push({ key: p }); // the folder placeholder, if any
    }
    deleteProgress = `${count(list.length)} 件を削除中…`;
    try {
      const errors = await s3.deleteObjects(bucket, list, { bypassGovernance: bypass });
      const real = errors.filter((e) => e.code !== 'NoSuchKey');
      if (real.length) {
        toast(`${real.length} 件を削除できませんでした（例: ${real[0].key}: ${real[0].message ?? real[0].code}）`, 'bad');
      } else {
        toast(versions ? `${list.length} 件のバージョンを完全に削除しました` : `${list.length} 件を削除しました`);
      }
    } finally {
      deleteProgress = null;
      await load();
    }
  }
  const deleteCount = $derived(deleteTargets.objects.length + deleteTargets.folders.length);
</script>

<div
  class="drop-root"
  class:dragging
  role="region"
  aria-label="オブジェクト一覧"
  ondragover={(e) => {
    if (versions) return;
    e.preventDefault();
    dragging = true;
  }}
  ondragleave={(e) => {
    if (e.currentTarget === e.target) dragging = false;
  }}
  ondrop={(e) => (versions ? null : onDrop(e))}
>
  <PageHead {title} crumbs={crumbs}>
    <a class="button" href={link(['b', bucket, 'settings'])}>バケットの設定</a>
  </PageHead>

  <section class="sheet">
    <header class="toolbar">
      <div class="row">
        {#if !versions}
          <button class="primary" onclick={() => fileInput.click()}>ファイルをアップロード</button>
          <button onclick={() => dirInput.click()}>フォルダをアップロード</button>
          <button onclick={() => (folderOpen = true)}>フォルダを作成</button>
        {/if}
        <button class="danger" disabled={selected.size === 0} onclick={askDeleteSelected}>
          選択を削除{selected.size ? `（${selected.size}）` : ''}
        </button>
      </div>
      <div class="row">
        <input type="search" placeholder="このフォルダ内で絞り込み" bind:value={filter} aria-label="このフォルダ内で絞り込み" />
        {#if versioning}
          <label class="check small"><input type="checkbox" checked={versions} onchange={(e) => setVersions(e.target.checked)} /> すべてのバージョンを表示</label>
        {/if}
        <button class="quiet" onclick={() => load()} disabled={loading}>再読み込み</button>
      </div>
    </header>

    {#if !versions}
      <input class="hidden" type="file" multiple bind:this={fileInput} onchange={(e) => onPick(e, false)} />
      <input class="hidden" type="file" webkitdirectory multiple bind:this={dirInput} onchange={(e) => onPick(e, true)} />
    {/if}

    {#if error}
      <div class="body"><p class="notice bad">{error}</p></div>
    {:else if !loading && folders.length === 0 && objects.length === 0}
      <div class="empty">
        {#if prefix}
          <h2>このフォルダは空です</h2>
        {:else}
          <h2>まだオブジェクトがありません</h2>
        {/if}
        {#if !versions}<p>ファイルやフォルダをここにドラッグ＆ドロップするか、「ファイルをアップロード」を押してください。</p>{/if}
      </div>
    {:else}
      <div class="table-wrap">
        <table class="grid">
          <thead>
            <tr>
              <th class="sel"><input type="checkbox" aria-label="すべて選択" checked={allSelected} onchange={toggleAll} /></th>
              <th>名前</th>
              {#if versions}<th>バージョン</th>{/if}
              <th class="num">サイズ</th>
              <th>更新日時</th>
              <th>ストレージクラス</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {#each shownFolders as p (p)}
              <tr>
                <td class="sel"></td>
                <td colspan={versions ? 5 : 4}>
                  <a class="folder" href={link(['b', bucket], { prefix: p, versions: versions ? '1' : undefined })}>
                    <span class="icon" aria-hidden="true"></span>{basename(p)}
                  </a>
                </td>
                <td class="actions">
                  {#if !versions}<button class="danger small" onclick={() => askDelete([], [p])}>削除</button>{/if}
                </td>
              </tr>
            {/each}
            {#each shownObjects as o (rowId(o))}
              <tr class:marker={o.deleteMarker} class:old={versions && !o.isLatest}>
                <td class="sel">
                  <input type="checkbox" aria-label={`${basename(o.key)} を選択`} checked={selected.has(rowId(o))} onchange={() => toggle(rowId(o))} />
                </td>
                <td class="name">
                  {#if o.deleteMarker}
                    <span class="break">{basename(o.key)}</span>
                  {:else}
                    <button class="link break" onclick={() => (detail = { key: o.key, versionId: versions ? o.versionId : undefined })}>{basename(o.key)}</button>
                  {/if}
                </td>
                {#if versions}
                  <td class="small">
                    {#if o.deleteMarker}<span class="badge bad">削除マーカー</span>{/if}
                    {#if o.isLatest}<span class="badge info">最新</span>{/if}
                    <span class="mono faint" title={o.versionId}>{o.versionId === 'null' ? 'null' : `…${o.versionId.slice(-8)}`}</span>
                  </td>
                {/if}
                <td class="num">{o.deleteMarker ? '—' : bytes(o.size)}</td>
                <td class="muted">{datetime(o.lastModified)}</td>
                <td class="small muted">{o.deleteMarker ? '' : (o.storageClass ?? 'STANDARD')}</td>
                <td class="actions">
                  {#if !o.deleteMarker}
                    <a class="button small" href={s3.downloadUrl(bucket, o.key, versions ? o.versionId : undefined)} download>ダウンロード</a>
                  {/if}
                  <button class="danger small" onclick={() => askDelete([o])}>削除</button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      {#if truncated}
        <div class="more"><button onclick={() => load(true)} disabled={loading}>{loading ? '読み込み中…' : 'さらに読み込む'}</button></div>
      {/if}
    {/if}
    {#if loading && objects.length === 0 && folders.length === 0}<div class="body faint">読み込み中…</div>{/if}
  </section>

  {#if uploads.length}
    <section class="sheet uploads" aria-label="アップロード">
      <header>
        <h3>アップロード</h3>
        <div class="row">
          {#if info.storage_classes?.length}
            <label class="small muted">保存先クラス
              <select bind:value={storageClass} disabled={uploading}>
                <option value="">バケットの既定</option>
                {#each info.storage_classes as c (c.name)}<option value={c.name}>{c.name}（コピー {c.replicas}）</option>{/each}
              </select>
            </label>
          {/if}
          <button class="small" onclick={clearFinished} disabled={!uploads.some((u) => u.status !== 'waiting' && u.status !== 'sending')}>完了したものを消す</button>
        </div>
      </header>
      <ul>
        {#each uploads as u (u.id)}
          <li>
            <div class="spread">
              <span class="break">{u.name}</span>
              <span class="row small">
                {#if u.status === 'done'}<span class="badge ok">完了</span>
                {:else if u.status === 'failed'}<span class="badge bad" title={u.error}>失敗</span>
                {:else if u.status === 'cancelled'}<span class="badge">中止</span>
                {:else if u.status === 'waiting'}<span class="faint">待機中</span>
                {:else}<span class="muted">{bytes(u.sent)} / {bytes(u.size)}</span>{/if}
                {#if u.status === 'waiting' || u.status === 'sending'}<button class="quiet small" onclick={() => cancel(u)}>中止</button>{/if}
              </span>
            </div>
            {#if u.status === 'sending'}<div class="meter"><span style="width: {u.size ? (100 * u.sent) / u.size : 100}%"></span></div>{/if}
            {#if u.status === 'failed'}<p class="small" style="color: var(--shu)">{u.error}</p>{/if}
          </li>
        {/each}
      </ul>
    </section>
  {:else if !versions}
    <p class="faint small drop-hint">ファイルやフォルダはこの画面にドラッグ＆ドロップしてもアップロードできます（{bytes(s3.MULTIPART_THRESHOLD)} 以上は分割して送信します）。</p>
  {/if}
</div>

{#if detail}
  <ObjectPanel {bucket} objectKey={detail.key} versionId={detail.versionId} {lock} {info} onclose={() => (detail = null)} onchange={() => load()} />
{/if}

<Modal bind:open={folderOpen} title="フォルダを作成">
  <form id="new-folder" class="stack" onsubmit={createFolder}>
    <label class="field">
      <span>フォルダ名</span>
      <input type="text" bind:value={folderName} required autocomplete="off" />
      <span class="hint">作成場所: <code>{bucket}/{prefix}</code>。S3 にフォルダはなく、名前が「/」で終わる空のオブジェクトを作ります。</span>
    </label>
  </form>
  {#snippet footer()}
    <button onclick={() => (folderOpen = false)}>キャンセル</button>
    <button class="primary" type="submit" form="new-folder">作成</button>
  {/snippet}
</Modal>

<Confirm bind:open={deleteOpen} title={versions ? 'バージョンを完全に削除' : '削除'} action={deleteProgress ?? '削除'} onconfirm={runDelete}>
  {#if versions}
    <p>選んだ {deleteCount} 件のバージョンを完全に削除します。元に戻せません。</p>
  {:else if versioning === 'Enabled'}
    <p>{deleteCount} 件を削除します。バージョニングが有効なので削除マーカーが付き、以前の版は「すべてのバージョンを表示」から復元できます。</p>
  {:else}
    <p>{deleteCount} 件を削除します。元に戻せません。</p>
  {/if}
  {#if deleteTargets.folders.length}
    <p class="notice warn">フォルダの中身もすべて削除します: {deleteTargets.folders.map(basename).join('、')}</p>
  {/if}
  {#if lock && versions}
    <label class="check small"><input type="checkbox" bind:checked={bypass} /> ガバナンスモードの保持を解除して削除する</label>
  {/if}
</Confirm>

<style>
  .drop-root {
    position: relative;
    min-height: 60vh;
  }
  .drop-root.dragging::after {
    content: 'ここにドロップしてアップロード';
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    font-size: 1.2rem;
    font-weight: 600;
    color: var(--indigo);
    border: 2px dashed var(--indigo);
    border-radius: var(--radius-m);
    background: color-mix(in srgb, var(--indigo-soft) 85%, transparent);
    pointer-events: none;
  }
  .toolbar {
    flex-wrap: wrap;
  }
  .hidden {
    display: none;
  }
  .sel {
    width: 2.2rem;
  }
  .folder {
    display: inline-flex;
    gap: 0.45rem;
    align-items: center;
    font-weight: 600;
    color: var(--ink);
    text-decoration: none;
  }
  .folder:hover {
    color: var(--indigo);
  }
  /* A folder tab drawn in CSS (no icon font). */
  .icon {
    width: 14px;
    height: 10px;
    border: 1.5px solid var(--kincha);
    border-radius: 1px 2px 2px 2px;
    position: relative;
    flex: none;
  }
  .icon::before {
    content: '';
    position: absolute;
    left: -1.5px;
    top: -4px;
    width: 6px;
    height: 3px;
    border: 1.5px solid var(--kincha);
    border-bottom: none;
    border-radius: 1px 1px 0 0;
  }
  td.name {
    min-width: 10rem;
  }
  button.link {
    border: none;
    background: none;
    padding: 0;
    color: var(--indigo);
    text-align: left;
    white-space: normal;
    font-size: inherit;
  }
  button.link:hover {
    text-decoration: underline;
  }
  tr.marker td,
  tr.old td {
    color: var(--ink-faint);
  }
  .more {
    padding: 0.8rem;
    text-align: center;
    border-top: 1px solid var(--line);
  }
  .uploads {
    margin-top: 1.2rem;
  }
  .uploads ul {
    list-style: none;
    margin: 0;
    padding: 0.4rem 1.1rem 0.9rem;
    display: grid;
    gap: 0.6rem;
    max-height: 320px;
    overflow-y: auto;
  }
  .uploads li {
    display: grid;
    gap: 0.3rem;
    font-size: 0.9rem;
  }
  .drop-hint {
    margin-top: 0.8rem;
  }
</style>
