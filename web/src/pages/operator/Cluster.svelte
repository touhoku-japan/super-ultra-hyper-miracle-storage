<script>
  import PageHead from '../../components/PageHead.svelte';
  import Confirm from '../../components/Confirm.svelte';
  import { admin } from '../../lib/admin.js';
  import { bytes, count, ago, datetime } from '../../lib/format.js';
  import { toast } from '../../lib/toast.svelte.js';

  let disks = $state(null);
  let error = $state(null);

  async function load() {
    try {
      disks = await admin.disks();
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

  const nodes = $derived.by(() => {
    const m = new Map();
    for (const d of disks ?? []) {
      if (!m.has(d.node_id)) m.set(d.node_id, []);
      m.get(d.node_id).push(d);
    }
    return [...m.entries()].sort(([a], [b]) => a.localeCompare(b));
  });

  // One confirm dialog for every action: { title, message, action, confirmText, run }
  let pending = $state(null);
  let open = $state(false);
  let force = $state(false);
  function ask(p) {
    pending = p;
    force = false;
    open = true;
  }

  function report(r, what) {
    toast(`${what}（repair 待ちに ${count(r.queued)} 件追加）`);
    if (r.warning) toast(r.warning, 'bad');
  }

  const drainDisk = (d) =>
    ask({
      title: 'ディスクを撤去中にする',
      message: `${d.path}（${d.node_id}）に新しいコピーを置かないようにし、repair がこのディスクのコピーを他へ移します。移し終えたら「登録を削除」できます。`,
      action: '撤去中にする',
      run: async () => report(await admin.drainDisk(d.id), 'ディスクを撤去中にしました'),
    });
  const undrainDisk = (d) =>
    ask({
      title: '撤去を取り消す',
      message: `${d.path} を通常のディスクに戻します。すでに他へ作ったコピーは repair が余分な分を減らします。`,
      action: '取り消す',
      danger: false,
      run: async () => report(await admin.undrainDisk(d.id), '撤去を取り消しました'),
    });
  const removeDisk = (d) =>
    ask({
      title: 'ディスクの登録を削除',
      message: `${d.path}（${d.node_id}）を登録から外します。先にノードの設定（[storage] disks）からも外してください（起動時に再登録されます）。`,
      action: '登録を削除',
      confirmText: d.path,
      forceable: d.copies > 0,
      copies: d.copies,
      run: async () => {
        const r = await admin.removeDisk(d.id, force);
        toast(r.dropped ? `登録を削除しました（${count(r.dropped)} 件のコピー記録を破棄し、再複製を予約）` : '登録を削除しました');
      },
    });
  const drainNode = (n) =>
    ask({
      title: 'ノードを撤去中にする',
      message: `ノード ${n} のすべてのディスクを撤去中にします。`,
      action: '撤去中にする',
      run: async () => report(await admin.drainNode(n), `ノード ${n} を撤去中にしました`),
    });
  const undrainNode = (n) =>
    ask({
      title: 'ノードの撤去を取り消す',
      message: `ノード ${n} のすべてのディスクを通常に戻します。`,
      action: '取り消す',
      danger: false,
      run: async () => report(await admin.undrainNode(n), `ノード ${n} の撤去を取り消しました`),
    });
  const removeNode = (n, ds) => {
    const copies = ds.reduce((a, d) => a + d.copies, 0);
    ask({
      title: 'ノードの登録を削除',
      message: `ノード ${n} のすべてのディスクを登録から外します。先にクラスタの設定（[cluster] nodes）からも外してください。`,
      action: '登録を削除',
      confirmText: n,
      forceable: copies > 0,
      copies,
      run: async () => {
        const r = await admin.removeNode(n, force);
        toast(r.dropped ? `ノード ${n} を削除しました（${count(r.dropped)} 件のコピー記録を破棄し、再複製を予約）` : `ノード ${n} を削除しました`);
      },
    });
  };

  async function confirmed() {
    await pending.run();
    await load();
  }
</script>

<PageHead title="ディスクとノード">
  <button onclick={load}>再読み込み</button>
</PageHead>

{#if error}
  <p class="notice bad">{error}</p>
{:else if !disks}
  <p class="faint">読み込み中…</p>
{:else}
  <p class="muted small intro">各ノードは 10 秒ごとに自分のディスクの空き容量を書き込みます。30 秒更新がないディスクは停止とみなし、repair が足りないコピーを他のノードに作り直します。</p>
  {#each nodes as [node, ds] (node)}
    {@const allDraining = ds.every((d) => d.draining)}
    {@const up = ds.filter((d) => d.up).length}
    <section class="sheet node">
      <header>
        <div class="row">
          <h2>{node}</h2>
          {#if up === ds.length}<span class="badge ok">稼働中</span>{:else if up === 0}<span class="badge bad">停止</span>{:else}<span class="badge warn">{ds.length - up} 本停止</span>{/if}
          {#if allDraining}<span class="badge warn">撤去中</span>{/if}
        </div>
        <div class="row">
          {#if allDraining}
            <button class="small" onclick={() => undrainNode(node)}>撤去を取り消す</button>
            <button class="danger small" onclick={() => removeNode(node, ds)}>登録を削除</button>
          {:else}
            <button class="small" onclick={() => drainNode(node)}>ノードを撤去中にする</button>
          {/if}
        </div>
      </header>
      <div class="table-wrap">
        <table class="grid">
          <thead><tr><th>パス</th><th>状態</th><th class="num">空き容量</th><th class="num">コピー数</th><th>最終応答</th><th></th></tr></thead>
          <tbody>
            {#each ds as d (d.id)}
              <tr>
                <td><span class="mono">{d.path}</span><br /><span class="mono faint small">{d.id}</span></td>
                <td>
                  {#if d.up}<span class="badge ok">稼働</span>{:else}<span class="badge bad">停止</span>{/if}
                  {#if d.draining}<span class="badge warn">撤去中</span>{/if}
                  {#if d.down_since}<br /><span class="faint small">{datetime(d.down_since)} に停止を検知</span>{/if}
                </td>
                <td class="num">{bytes(d.free_bytes)}</td>
                <td class="num">{count(d.copies)}</td>
                <td class="muted small">{ago(d.heartbeat_at)}</td>
                <td class="actions">
                  {#if d.draining}
                    <button class="small" onclick={() => undrainDisk(d)}>撤去を取り消す</button>
                    <button class="danger small" onclick={() => removeDisk(d)}>登録を削除</button>
                  {:else}
                    <button class="small" onclick={() => drainDisk(d)}>撤去中にする</button>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </section>
  {/each}
{/if}

<Confirm bind:open title={pending?.title} action={pending?.action} danger={pending?.danger !== false} confirmText={pending?.confirmText} onconfirm={confirmed}>
  <p>{pending?.message}</p>
  {#if pending?.forceable}
    <p class="notice warn">まだ {count(pending.copies)} 件のコピーが残っています。repair が移し終えるまで待ってください。</p>
    <label class="check small"><input type="checkbox" bind:checked={force} /> ディスクが壊れていて戻らないので強制的に削除する（コピー記録を破棄し、他のコピーから作り直す）</label>
  {/if}
</Confirm>

<style>
  .intro {
    margin-bottom: 1.2rem;
  }
  .node + .node {
    margin-top: 1.2rem;
  }
</style>
