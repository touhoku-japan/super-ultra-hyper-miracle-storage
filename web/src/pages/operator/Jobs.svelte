<script>
  import PageHead from '../../components/PageHead.svelte';
  import { admin } from '../../lib/admin.js';
  import { datetime, bytes, count, duration } from '../../lib/format.js';
  import { toast } from '../../lib/toast.svelte.js';

  let { info } = $props();
  let jobs = $state(null);
  let error = $state(null);

  const ABOUT = {
    scrub: { title: 'scrub（全コピーの検証）', text: 'このノードのディスクにある全コピーを読み直してハッシュを照合し、壊れたコピーを捨てます（repair が作り直します）。ディスクを全部読むので時間がかかります。', scope: 'このノード' },
    repair: { title: 'repair（キューの処理）', text: 'コピー数が足りない・多すぎる・撤去中のディスクにあるオブジェクトを直します。通常は定期実行されます。', scope: 'クラスタで 1 台' },
    'repair-full': { title: 'repair（全件確認）', text: 'キューに加えて全オブジェクトのコピー数を確認します。大規模な環境では重い処理です。', scope: 'クラスタで 1 台' },
    gc: { title: 'gc（孤立ファイルの回収）', text: 'このノードのディスクで、どのオブジェクトにも使われていないファイルをゴミ箱へ移し、猶予を過ぎたゴミ箱を空にします。', scope: 'このノード' },
    rebalance: { title: 'rebalance（容量の平準化）', text: '空きの少ないディスクから多いディスクへコピーを移します。ディスクやノードを追加した後に実行してください。', scope: 'クラスタで 1 台' },
    'checksum-backfill': { title: 'チェックサムの後付け', text: 'チェックサムを持たない古いオブジェクトに CRC64NVME を付けます。', scope: 'クラスタで 1 台' },
  };

  const LABELS = {
    checked: '確認', removed: '破棄', copied: 'コピー作成', trimmed: '余分を削除', drained: '撤去中から移動',
    queued: 'キューに追加', postponed: '後回し', resumed: '再開', unrecoverable: '修復不能', orphans: '孤立ファイル',
    uploads: '放置アップロード', tmp: '一時ファイル', purged: 'ゴミ箱から削除', skipped_disks: 'スキップしたディスク',
    moved: '移動', updated: '付与', corrupt: '破損', failed: '失敗', remaining: '残りあり',
  };
  const fmt = (k, v) => (k.endsWith('bytes') ? bytes(v) : typeof v === 'boolean' ? (v ? 'はい' : 'いいえ') : count(v));

  let params = $state({ gc: { grace_hours: '', force: false }, rebalance: { margin_gib: 10, max_moves: '' }, 'checksum-backfill': { max_objects: '' } });

  async function load() {
    try {
      jobs = await admin.jobs();
      error = null;
    } catch (e) {
      error = e.message;
    }
  }
  load();
  $effect(() => {
    const t = setInterval(load, jobs?.some((j) => j.runs.some((r) => r.running)) ? 2000 : 10000);
    return () => clearInterval(t);
  });

  function body(name) {
    const n = (v) => (v === '' || v === null || v === undefined ? undefined : Number(v));
    if (name === 'gc') {
      const h = n(params.gc.grace_hours);
      return { grace_secs: h === undefined ? undefined : Math.round(h * 3600), force: params.gc.force };
    }
    if (name === 'rebalance') return { margin_gib: n(params.rebalance.margin_gib), max_moves: n(params.rebalance.max_moves) };
    if (name === 'checksum-backfill') return { max_objects: n(params['checksum-backfill'].max_objects) };
    return undefined;
  }

  async function start(name) {
    try {
      await admin.startJob(name, body(name));
      toast(`${ABOUT[name]?.title ?? name} を開始しました`);
      await load();
    } catch (e) {
      toast(e.message, 'bad');
    }
  }
</script>

<PageHead title="メンテナンス" />
<p class="muted small intro">各ノードの最後の実行（定期実行を含む）を表示します。ここで開始した処理はノード <code>{info.node_id}</code> のサーバ内で動きます。「クラスタで 1 台」の処理は、どこかのノードで実行中のあいだは開始できません。定期実行の間隔は設定ファイルの <code>[maintenance]</code> で決まります。</p>

{#if error}
  <p class="notice bad">{error}</p>
{:else if !jobs}
  <p class="faint">読み込み中…</p>
{:else}
  <div class="jobs">
    {#each jobs as j (j.name)}
      {@const a = ABOUT[j.name] ?? { title: j.name, text: '', scope: '' }}
      {@const here = j.runs.find((r) => r.this_node && r.running)}
      {@const elsewhere = j.runs.find((r) => !r.this_node && r.running)}
      <section class="sheet">
        <header>
          <h2>{a.title}</h2>
          <span class="badge">{a.scope}</span>
        </header>
        <div class="body stack">
          <p class="muted small">{a.text}</p>
          {#if j.name === 'gc'}
            <div class="row small">
              <label>猶予（時間） <input type="number" min="0" bind:value={params.gc.grace_hours} placeholder="既定" style="width: 6rem" /></label>
              <label class="check"><input type="checkbox" bind:checked={params.gc.force} /> コピー記録のないディスクでも実行</label>
            </div>
          {:else if j.name === 'rebalance'}
            <div class="row small">
              <label>差の許容（GiB） <input type="number" min="0" bind:value={params.rebalance.margin_gib} style="width: 5rem" /></label>
              <label>最大移動数 <input type="number" min="1" bind:value={params.rebalance.max_moves} placeholder="無制限" style="width: 6rem" /></label>
            </div>
          {:else if j.name === 'checksum-backfill'}
            <div class="row small">
              <label>最大件数 <input type="number" min="1" bind:value={params['checksum-backfill'].max_objects} placeholder="無制限" style="width: 7rem" /></label>
            </div>
          {/if}
          <div class="spread">
            <button class="primary" disabled={j.blocked} onclick={() => start(j.name)}>{here ? '実行中…' : '今すぐ実行'}</button>
            {#if !here && j.blocked && elsewhere}<span class="faint small">ノード {elsewhere.node_id} で実行中</span>{/if}
          </div>
          {#if j.runs.length === 0}
            <p class="faint small">まだ実行されていません。</p>
          {:else}
            <ul class="runs">
              {#each j.runs as r (r.node_id)}
                <li>
                  <div class="spread">
                    <span class="small node"><strong class="mono">{r.node_id}</strong>{#if r.this_node}<span class="badge info">このノード</span>{/if}</span>
                    {#if r.running}<span class="badge warn">実行中</span>{:else if r.interrupted}<span class="badge bad">中断</span>{:else if r.error}<span class="badge bad">失敗</span>{:else}<span class="badge ok">完了</span>{/if}
                  </div>
                  <p class="faint small">
                    {r.started_by === 'console' ? 'コンソールから開始' : '定期実行'}、
                    {#if r.running || r.finished_at == null}{datetime(r.started_at)} に開始{:else}{datetime(r.finished_at)} に終了（{duration(r.finished_at - r.started_at)}）{/if}
                  </p>
                  {#if !r.running && r.error}
                    <p class="notice bad small">{r.error}</p>
                  {:else if !r.running && r.result}
                    <dl class="props result">
                      {#each Object.entries(r.result) as [k, v] (k)}<dt>{LABELS[k] ?? k}</dt><dd>{fmt(k, v)}</dd>{/each}
                    </dl>
                  {/if}
                </li>
              {/each}
            </ul>
          {/if}
        </div>
      </section>
    {/each}
  </div>
{/if}

<style>
  .intro {
    margin-bottom: 1.2rem;
  }
  .jobs {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(340px, 1fr));
    gap: 1.2rem;
    align-items: start;
  }
  .runs {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
  }
  .runs li {
    display: grid;
    gap: 0.35rem;
    padding: 0.7rem 0;
    border-top: 1px solid var(--line);
  }
  .runs p {
    margin: 0;
  }
  .node {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
  }
  .result {
    font-size: 0.85rem;
    grid-template-columns: max-content 1fr max-content 1fr;
  }
</style>
