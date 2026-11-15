<script>
  // Side panel for one object (or one version of it).
  import Confirm from '../../components/Confirm.svelte';
  import * as s3 from '../../lib/s3.js';
  import { api } from '../../lib/api.js';
  import { bytes, datetime, basename } from '../../lib/format.js';
  import { toast } from '../../lib/toast.svelte.js';

  let { bucket, objectKey, versionId, lock, info, onclose, onchange } = $props();

  let head = $state(null);
  let error = $state(null);
  let tags = $state([]);
  let tagsDirty = $state(false);
  let versions = $state(null);
  let panel;

  async function load() {
    error = null;
    try {
      head = await s3.headObject(bucket, objectKey, versionId);
      tags = await s3.getTags(bucket, objectKey, versionId).catch(() => []);
      tagsDirty = false;
      const page = await s3.listVersions(bucket, { prefix: objectKey }).catch(() => null);
      versions = page ? page.entries.filter((v) => v.key === objectKey) : null;
    } catch (e) {
      error = s3.errorMessage(e);
    }
  }
  load();

  $effect(() => {
    panel?.focus();
    const onKey = (e) => e.key === 'Escape' && onclose();
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });

  // ---- tags ----
  function addTag() {
    tags.push({ key: '', value: '' });
    tagsDirty = true;
  }
  async function saveTags() {
    const clean = tags.filter((t) => t.key.trim());
    try {
      await s3.putTags(bucket, objectKey, versionId, clean.map((t) => ({ key: t.key.trim(), value: t.value })));
      toast('タグを保存しました');
      await load();
    } catch (e) {
      toast(s3.errorMessage(e), 'bad');
    }
  }

  // ---- share ----
  let shareSecs = $state(3600);
  let shareUrl = $state(null);
  async function share() {
    try {
      const r = await api('api/presign', {
        method: 'POST',
        body: { bucket, key: objectKey, version_id: versionId ?? null, expires_secs: Number(shareSecs) },
      });
      shareUrl = r.url;
    } catch (e) {
      toast(e.message, 'bad');
    }
  }
  async function copyShare() {
    try {
      await navigator.clipboard.writeText(shareUrl);
      toast('リンクをコピーしました');
    } catch {
      toast('コピーできませんでした。リンクを選択してコピーしてください', 'bad');
    }
  }

  // ---- versions ----
  let restoring = $state(null);
  async function restore(v) {
    restoring = v.versionId;
    try {
      await s3.copyObject(bucket, objectKey, { bucket, key: objectKey, versionId: v.versionId });
      toast('この版を最新として復元しました');
      onchange();
      await load();
    } catch (e) {
      toast(s3.errorMessage(e), 'bad');
    } finally {
      restoring = null;
    }
  }

  let delOpen = $state(false);
  let delTarget = $state(null); // version entry or null (= this object)
  let bypass = $state(false);
  async function runDelete() {
    const vid = delTarget ? delTarget.versionId : versionId;
    await s3.deleteObject(bucket, objectKey, { versionId: vid, bypassGovernance: bypass });
    toast(vid ? 'バージョンを削除しました' : '削除しました');
    onchange();
    if (!delTarget || delTarget.versionId === versionId) onclose();
    else await load();
  }

  // ---- lock ----
  let retMode = $state('GOVERNANCE');
  let retUntil = $state('');
  let retBypass = $state(false);
  async function saveRetention() {
    if (!retUntil) return;
    try {
      await s3.putRetention(bucket, objectKey, versionId ?? head?.versionId, retMode, new Date(retUntil).toISOString(), { bypassGovernance: retBypass });
      toast('保持期間を設定しました');
      await load();
    } catch (e) {
      toast(s3.errorMessage(e), 'bad');
    }
  }
  async function setHold(on) {
    try {
      await s3.putLegalHold(bucket, objectKey, versionId ?? head?.versionId, on);
      toast(on ? 'リーガルホールドを掛けました' : 'リーガルホールドを外しました');
      await load();
    } catch (e) {
      toast(s3.errorMessage(e), 'bad');
    }
  }

  const SHARE_CHOICES = [
    [3600, '1 時間'],
    [86400, '1 日'],
    [7 * 86400, '7 日'],
  ];
</script>

<div class="overlay" onclick={onclose} aria-hidden="true"></div>
<div class="panel" role="dialog" aria-modal="true" aria-label={basename(objectKey)} tabindex="-1" bind:this={panel}>
  <header>
    <div>
      <h2 class="break">{basename(objectKey)}</h2>
      <p class="faint small break">{bucket}/{objectKey}</p>
    </div>
    <button class="quiet" aria-label="閉じる" onclick={onclose}>×</button>
  </header>

  {#if error}
    <p class="notice bad">{error}</p>
  {:else if !head}
    <p class="faint">読み込み中…</p>
  {:else}
    <div class="row">
      <a class="button primary" href={s3.downloadUrl(bucket, objectKey, versionId)} download>ダウンロード</a>
      <button class="danger" onclick={() => { delTarget = null; bypass = false; delOpen = true; }}>削除</button>
    </div>

    <section>
      <h3>プロパティ</h3>
      <dl class="props">
        <dt>サイズ</dt><dd>{bytes(head.size)}（{head.size.toLocaleString('ja-JP')} バイト）</dd>
        <dt>種類</dt><dd>{head.contentType ?? '—'}</dd>
        <dt>更新日時</dt><dd>{datetime(head.lastModified)}</dd>
        <dt>ストレージクラス</dt><dd>{head.storageClass}</dd>
        <dt>ETag</dt><dd class="mono">{head.etag}</dd>
        {#if head.versionId}<dt>バージョン ID</dt><dd class="mono">{head.versionId}</dd>{/if}
        {#each Object.entries(head.checksums) as [algo, value] (algo)}
          <dt>{algo}</dt><dd class="mono">{value}{head.checksumType ? `（${head.checksumType}）` : ''}</dd>
        {/each}
      </dl>
    </section>

    {#if Object.keys(head.meta).length}
      <section>
        <h3>ユーザーメタデータ</h3>
        <dl class="props">
          {#each Object.entries(head.meta) as [k, v] (k)}<dt>{k}</dt><dd>{v}</dd>{/each}
        </dl>
      </section>
    {/if}

    <section>
      <div class="spread"><h3>タグ</h3><button class="small" onclick={addTag} disabled={tags.length >= 10}>タグを追加</button></div>
      {#if tags.length === 0}
        <p class="faint small">タグはありません。ライフサイクルの対象を絞り込むのに使えます（最大 10 個）。</p>
      {:else}
        <div class="tags">
          {#each tags as t, i (i)}
            <input type="text" placeholder="キー" bind:value={t.key} oninput={() => (tagsDirty = true)} maxlength="128" aria-label="タグのキー" />
            <input type="text" placeholder="値" bind:value={t.value} oninput={() => (tagsDirty = true)} maxlength="256" aria-label="タグの値" />
            <button class="quiet small" aria-label="タグを削除" onclick={() => { tags.splice(i, 1); tagsDirty = true; }}>×</button>
          {/each}
        </div>
      {/if}
      {#if tagsDirty}<button class="primary small" onclick={saveTags}>タグを保存</button>{/if}
    </section>

    {#if info.share_links}
      <section>
        <h3>共有リンク</h3>
        <p class="faint small">ログインしていない人でもダウンロードできる期限付きの URL を作ります。アクセスキーを無効にするとリンクも使えなくなります。</p>
        <div class="row">
          <select bind:value={shareSecs} aria-label="有効期限">
            {#each SHARE_CHOICES as [s, label] (s)}<option value={s}>{label}</option>{/each}
          </select>
          <button onclick={share}>リンクを作成</button>
        </div>
        {#if shareUrl}
          <textarea readonly rows="3" onfocus={(e) => e.target.select()}>{shareUrl}</textarea>
          <button class="small" onclick={copyShare}>コピー</button>
        {/if}
      </section>
    {/if}

    {#if lock}
      <section>
        <h3>オブジェクトロック</h3>
        <dl class="props">
          <dt>保持</dt>
          <dd>{#if head.lockMode}<span class="badge warn">{head.lockMode}</span> {datetime(head.retainUntil)} まで{:else}なし{/if}</dd>
          <dt>リーガルホールド</dt>
          <dd>
            {#if head.legalHold}<span class="badge warn">ON</span>{:else}OFF{/if}
            <button class="small" onclick={() => setHold(!head.legalHold)}>{head.legalHold ? '外す' : '掛ける'}</button>
          </dd>
        </dl>
        <div class="row">
          <select bind:value={retMode} aria-label="保持モード">
            <option value="GOVERNANCE">GOVERNANCE（権限があれば解除可）</option>
            <option value="COMPLIANCE">COMPLIANCE（期限まで誰も削除できない）</option>
          </select>
          <input type="datetime-local" bind:value={retUntil} aria-label="保持期限" />
          <button onclick={saveRetention} disabled={!retUntil}>保持期間を設定</button>
        </div>
        {#if head.lockMode === 'GOVERNANCE'}
          <label class="check small"><input type="checkbox" bind:checked={retBypass} /> ガバナンス保持の短縮・解除を許可する</label>
        {/if}
        <p class="faint small">COMPLIANCE の保持期間は延長しかできず、期限まで版を削除できません。</p>
      </section>
    {/if}

    {#if versions && versions.length > 1}
      <section>
        <h3>バージョン（{versions.length}）</h3>
        <ul class="versions">
          {#each versions as v (v.versionId)}
            <li class:current={v.versionId === (versionId ?? head.versionId)}>
              <div class="spread">
                <span class="small">
                  {#if v.isLatest}<span class="badge info">最新</span>{/if}
                  {#if v.deleteMarker}<span class="badge bad">削除マーカー</span>{/if}
                  {datetime(v.lastModified)} {v.deleteMarker ? '' : `・${bytes(v.size)}`}
                </span>
                <span class="row">
                  {#if !v.deleteMarker}
                    <a class="button small" href={s3.downloadUrl(bucket, objectKey, v.versionId)} download>ダウンロード</a>
                    {#if !v.isLatest}<button class="small" disabled={restoring === v.versionId} onclick={() => restore(v)}>この版に戻す</button>{/if}
                  {/if}
                  <button class="danger small" onclick={() => { delTarget = v; bypass = false; delOpen = true; }}>削除</button>
                </span>
              </div>
              <span class="mono faint small">{v.versionId}</span>
            </li>
          {/each}
        </ul>
      </section>
    {/if}
  {/if}
</div>

<Confirm bind:open={delOpen} title={delTarget || versionId ? 'バージョンを完全に削除' : '削除'} onconfirm={runDelete}>
  {#if delTarget || versionId}
    <p>この版を完全に削除します。元に戻せません。</p>
  {:else if versions && versions.length}
    <p>削除マーカーを付けます。以前の版は残り、あとで復元できます。</p>
  {:else}
    <p><strong>{basename(objectKey)}</strong> を削除します。元に戻せません。</p>
  {/if}
  {#if lock}
    <label class="check small"><input type="checkbox" bind:checked={bypass} /> ガバナンスモードの保持を解除して削除する</label>
  {/if}
</Confirm>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    background: rgba(25, 30, 36, 0.35);
    z-index: 20;
  }
  .panel {
    position: fixed;
    top: 0;
    right: 0;
    bottom: 0;
    width: min(540px, 100vw);
    background: var(--panel);
    border-left: 1px solid var(--line);
    z-index: 21;
    overflow-y: auto;
    padding: 1.2rem 1.3rem 3rem;
    display: grid;
    gap: 1.4rem;
    align-content: start;
    box-shadow: -12px 0 40px rgba(15, 20, 25, 0.2);
    outline: none;
  }
  header {
    display: flex;
    justify-content: space-between;
    gap: 1rem;
    align-items: start;
  }
  section {
    display: grid;
    gap: 0.6rem;
    border-top: 1px solid var(--line);
    padding-top: 1rem;
  }
  .tags {
    display: grid;
    grid-template-columns: 1fr 1fr auto;
    gap: 0.4rem;
  }
  .versions {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 0.5rem;
  }
  .versions li {
    display: grid;
    gap: 0.2rem;
    padding: 0.5rem 0.6rem;
    border: 1px solid var(--line);
    border-radius: var(--radius-s);
  }
  .versions li.current {
    border-color: var(--indigo);
  }
</style>
