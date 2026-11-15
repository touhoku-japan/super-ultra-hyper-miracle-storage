<script>
  import PageHead from '../../components/PageHead.svelte';
  import Modal from '../../components/Modal.svelte';
  import Confirm from '../../components/Confirm.svelte';
  import { api } from '../../lib/api.js';
  import * as s3 from '../../lib/s3.js';
  import { bucketNameError } from '../../lib/s3xml.js';
  import { link, go } from '../../lib/router.svelte.js';
  import { datetime } from '../../lib/format.js';
  import { toast } from '../../lib/toast.svelte.js';

  let { info } = $props();
  let buckets = $state(null);
  let error = $state(null);
  let filter = $state('');

  let createOpen = $state(false);
  let name = $state('');
  let versioning = $state(false);
  let objectLock = $state(false);
  let creating = $state(false);
  let createError = $state(null);
  const nameError = $derived(name ? bucketNameError(name) : null);

  let deleting = $state(null);
  let deleteOpen = $state(false);

  async function load() {
    try {
      buckets = await api('api/buckets');
      error = null;
    } catch (e) {
      error = e.message;
    }
  }

  async function create(e) {
    e.preventDefault();
    creating = true;
    createError = null;
    try {
      await s3.createBucket(name, { objectLock });
      if (versioning || objectLock) await s3.putVersioning(name, 'Enabled');
      toast(`バケット ${name} を作成しました`);
      createOpen = false;
      const created = name;
      name = '';
      versioning = false;
      objectLock = false;
      go(['b', created]);
    } catch (err) {
      createError = s3.errorMessage(err);
    } finally {
      creating = false;
    }
  }

  async function remove() {
    await s3.deleteBucket(deleting);
    toast(`バケット ${deleting} を削除しました`);
    await load();
  }

  const shown = $derived((buckets ?? []).filter((b) => b.name.includes(filter.trim())));
  const atLimit = $derived(info.tenant?.max_buckets != null && (buckets?.length ?? 0) >= info.tenant.max_buckets);

  load();
</script>

<PageHead title="バケット">
  <button class="primary" onclick={() => (createOpen = true)} disabled={atLimit} title={atLimit ? 'バケット数の上限に達しています' : undefined}>バケットを作成</button>
</PageHead>

{#if error}
  <p class="notice bad">{error}</p>
{:else if buckets === null}
  <p class="faint">読み込み中…</p>
{:else if buckets.length === 0}
  <div class="sheet empty">
    <h2>まだバケットがありません</h2>
    <p>ファイルを置く入れ物としてバケットを作成してください。名前は全利用者で一意です。</p>
    <button class="primary" onclick={() => (createOpen = true)}>バケットを作成</button>
  </div>
{:else}
  <section class="sheet">
    <header>
      <input type="search" placeholder="名前で絞り込み" bind:value={filter} aria-label="バケットを名前で絞り込み" />
      <span class="faint small">
        {buckets.length} 個{#if info.tenant?.max_buckets != null}（上限 {info.tenant.max_buckets} 個）{/if}
      </span>
    </header>
    <div class="table-wrap">
      <table class="grid">
        <thead><tr><th>名前</th><th>作成日時</th><th>公開</th><th>既定のコピー数</th><th></th></tr></thead>
        <tbody>
          {#each shown as b (b.name)}
            <tr>
              <td><a href={link(['b', b.name])}><strong>{b.name}</strong></a></td>
              <td class="muted">{datetime(b.created_at)}</td>
              <td>{#if b.public_read}<span class="badge warn">公開（誰でも読める）</span>{:else}<span class="faint small">非公開</span>{/if}</td>
              <td class="muted">{b.replicas ?? `${info.default_replicas}（標準）`}</td>
              <td class="actions">
                <a class="button small" href={link(['b', b.name, 'settings'])}>設定</a>
                <button class="danger small" onclick={() => { deleting = b.name; deleteOpen = true; }}>削除</button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  </section>
{/if}

<Modal bind:open={createOpen} title="バケットを作成">
  <form id="create-bucket" class="stack" onsubmit={create}>
    <label class="field">
      <span>バケット名</span>
      <input type="text" bind:value={name} required spellcheck="false" autocomplete="off" placeholder="例: backup-2026" />
      {#if nameError}<span class="hint" style="color: var(--shu)">{nameError}</span>{:else}<span class="hint">英小文字・数字・ハイフン、3〜63 文字。作成後は変更できません。</span>{/if}
    </label>
    <label class="check"><input type="checkbox" bind:checked={versioning} disabled={objectLock} /> バージョニングを有効にする（上書き・削除した版も残します）</label>
    <label class="check"><input type="checkbox" bind:checked={objectLock} /> オブジェクトロックを使う（作成時にしか有効にできません。バージョニングも有効になります）</label>
    {#if createError}<p class="notice bad">{createError}</p>{/if}
  </form>
  {#snippet footer()}
    <button onclick={() => (createOpen = false)}>キャンセル</button>
    <button class="primary" type="submit" form="create-bucket" disabled={creating || !name || nameError}>{creating ? '作成中…' : '作成'}</button>
  {/snippet}
</Modal>

<Confirm
  bind:open={deleteOpen}
  title="バケットを削除"
  message="空のバケットだけ削除できます。中身（すべてのバージョン）を先に削除してください。"
  confirmText={deleting}
  onconfirm={remove}
/>
