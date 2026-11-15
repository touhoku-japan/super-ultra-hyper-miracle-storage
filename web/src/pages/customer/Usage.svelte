<script>
  import PageHead from '../../components/PageHead.svelte';
  import { api } from '../../lib/api.js';
  import { bytes, count, datetime } from '../../lib/format.js';

  let u = $state(null);
  let error = $state(null);
  api('api/usage')
    .then((r) => (u = r))
    .catch((e) => (error = e.message));

  const quotaPct = $derived(u?.quota_bytes ? Math.min(100, (100 * u.logical_bytes) / u.quota_bytes) : null);
</script>

<PageHead title="使用量" />

{#if error}
  <p class="notice bad">{error}</p>
{:else if !u}
  <p class="faint">読み込み中…</p>
{:else}
  <div class="usage">
    <section class="sheet store">
      <div class="body stack">
        <span class="muted">保存しているデータ</span>
        <span class="big">{bytes(u.logical_bytes)}</span>
        {#if u.quota_bytes}
          <div class="meter" class:high={quotaPct > 90}><span style="width: {quotaPct}%"></span></div>
          <span class="small muted">上限 {bytes(u.quota_bytes)} のうち {quotaPct.toFixed(1)}%</span>
        {/if}
        <dl class="props">
          <dt>オブジェクト数</dt><dd>{count(u.objects)}（すべての版を含む）</dd>
          <dt>バケット数</dt><dd>{count(u.buckets)}{u.max_buckets != null ? ` / 上限 ${count(u.max_buckets)}` : ''}</dd>
          <dt>ディスク上の量</dt><dd>{bytes(u.stored_bytes)}（コピー数を掛けた量）</dd>
        </dl>
      </div>
    </section>
    <section class="sheet">
      <header><h2>今月の利用</h2><span class="faint small">{datetime(u.month_start)} から（UTC で月初）</span></header>
      <div class="body">
        <table class="grid">
          <tbody>
            <tr><td>書き込み・一覧のリクエスト（クラス A）</td><td class="num">{count(u.class_a_requests)} 回</td></tr>
            <tr><td>読み出しなどのリクエスト（クラス B）</td><td class="num">{count(u.class_b_requests)} 回</td></tr>
            <tr><td>アップロード量</td><td class="num">{bytes(u.ingress_bytes)}</td></tr>
            <tr><td>ダウンロード量</td><td class="num">{bytes(u.egress_bytes)}</td></tr>
          </tbody>
        </table>
        <p class="faint small" style="margin-top: 0.8rem">数分遅れて反映されます。請求額はポータルで確認してください。</p>
      </div>
    </section>
  </div>
{/if}

<style>
  .usage {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(320px, 1fr));
    gap: 1.2rem;
    align-items: start;
  }
  .big {
    font-size: 2.4rem;
    font-weight: 700;
    line-height: 1.1;
    font-variant-numeric: tabular-nums;
  }
</style>
