<script>
  import PageHead from '../../components/PageHead.svelte';
  import { admin } from '../../lib/admin.js';
  import { link } from '../../lib/router.svelte.js';
  import { bytes, count } from '../../lib/format.js';

  let { info } = $props();
  let o = $state(null);
  let error = $state(null);

  async function load() {
    try {
      o = await admin.overview();
      error = null;
    } catch (e) {
      error = e.message;
    }
  }
  load();
  $effect(() => {
    const t = setInterval(load, 15000);
    return () => clearInterval(t);
  });

  const downDisks = $derived(o ? o.disks - o.disks_up : 0);
</script>

<PageHead title="概要" />

{#if error}
  <p class="notice bad">{error}</p>
{:else if !o}
  <p class="faint">読み込み中…</p>
{:else}
  {#if downDisks > 0}
    <p class="notice bad">応答のないディスクが {downDisks} 本あります。<a href={link(['cluster'])}>ディスクとノード</a>を確認してください。足りないコピーは repair が他のディスクに作り直します。</p>
  {/if}
  <div class="health">
    <section class="sheet">
      <div class="body stack">
        <h2>ディスク</h2>
        <p class="figure"><strong>{o.disks_up}</strong> / {o.disks} 本が稼働中</p>
        <dl class="props">
          <dt>ノード</dt><dd>{o.nodes}</dd>
          <dt>撤去中（draining）</dt><dd>{o.disks_draining}</dd>
          <dt>空き容量</dt><dd>{bytes(o.free_bytes)}（稼働中のディスクの合計）</dd>
        </dl>
      </div>
    </section>
    <section class="sheet">
      <div class="body stack">
        <h2>複製の状態</h2>
        <p class="figure"><strong>{count(o.repair_due)}{o.repair_due_more ? '+' : ''}</strong> 件が repair 待ち</p>
        <p class="muted small">コピー数が足りない・多すぎる・撤去中のディスクにあるオブジェクトです。repair が定期的に処理します。今すぐ実行するには<a href={link(['jobs'])}>メンテナンス</a>から。</p>
      </div>
    </section>
    <section class="sheet">
      <div class="body stack">
        <h2>保存量</h2>
        <p class="figure"><strong>{bytes(o.logical_bytes)}</strong></p>
        <dl class="props">
          <dt>ディスク上</dt><dd>{bytes(o.stored_bytes)}（コピー込み）</dd>
          <dt>オブジェクト</dt><dd>{count(o.objects)}</dd>
          <dt>テナント</dt><dd>{count(o.tenants)}（停止中 {count(o.suspended_tenants)}）</dd>
        </dl>
      </div>
    </section>
  </div>
  <p class="faint small foot">このコンソールはノード <code>{o.node_id}</code> の管理 API に接続しています。scrub と gc はこのノードのディスクだけを対象にします。公開バケット: {o.allow_public_buckets ? '許可' : '無効（[s3] allow_public_buckets）'}</p>
{/if}

<style>
  .health {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
    gap: 1.2rem;
  }
  .figure {
    font-size: 1.05rem;
    color: var(--ink-soft);
  }
  .figure strong {
    font-size: 2.2rem;
    color: var(--ink);
    font-variant-numeric: tabular-nums;
    margin-right: 0.2rem;
  }
  .foot {
    margin-top: 1.4rem;
  }
</style>
