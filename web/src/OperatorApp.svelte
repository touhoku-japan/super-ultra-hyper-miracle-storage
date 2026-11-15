<script>
  import Shell from './components/Shell.svelte';
  import { segments, link } from './lib/router.svelte.js';
  import Overview from './pages/operator/Overview.svelte';
  import Tenants from './pages/operator/Tenants.svelte';
  import TenantDetail from './pages/operator/TenantDetail.svelte';
  import Cluster from './pages/operator/Cluster.svelte';
  import Jobs from './pages/operator/Jobs.svelte';

  let { info, onlogout } = $props();
  const seg = $derived(segments());
  const page = $derived(seg[0] === 'tenants' ? (seg[1] ? 'tenant' : 'tenants') : seg[0] === 'cluster' ? 'cluster' : seg[0] === 'jobs' ? 'jobs' : 'overview');
  const nav = $derived([
    { href: link([]), label: '概要', active: page === 'overview' },
    { href: link(['tenants']), label: 'テナント', active: page === 'tenants' || page === 'tenant' },
    { href: link(['cluster']), label: 'ディスクとノード', active: page === 'cluster' },
    { href: link(['jobs']), label: 'メンテナンス', active: page === 'jobs' },
  ]);
</script>

<Shell product="運用コンソール" who={`運用者（ノード ${info.node_id}）`} {nav} {onlogout}>
  {#if page === 'tenants'}
    <Tenants />
  {:else if page === 'tenant'}
    {#key seg[1]}<TenantDetail id={seg[1]} {info} />{/key}
  {:else if page === 'cluster'}
    <Cluster />
  {:else if page === 'jobs'}
    <Jobs {info} />
  {:else}
    <Overview {info} />
  {/if}
</Shell>
