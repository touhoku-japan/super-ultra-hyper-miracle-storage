<script>
  import Shell from './components/Shell.svelte';
  import { route, segments, link } from './lib/router.svelte.js';
  import Buckets from './pages/customer/Buckets.svelte';
  import Browser from './pages/customer/Browser.svelte';
  import BucketSettings from './pages/customer/BucketSettings.svelte';
  import Usage from './pages/customer/Usage.svelte';

  let { info, onlogout } = $props();
  const seg = $derived(segments());
  const page = $derived(seg[0] === 'b' && seg[1] ? (seg[2] === 'settings' ? 'settings' : 'browser') : seg[0] === 'usage' ? 'usage' : 'buckets');
  const nav = $derived([
    { href: link(['buckets']), label: 'バケット', active: page === 'buckets' || page === 'browser' || page === 'settings' },
    { href: link(['usage']), label: '使用量', active: page === 'usage' },
  ]);
</script>

<Shell product={info.tenant?.name ?? ''} who={info.access_key_id} {nav} {onlogout}>
  {#if page === 'browser'}
    {#key seg[1]}
      <Browser bucket={seg[1]} prefix={route.query.prefix ?? ''} versions={route.query.versions === '1'} {info} />
    {/key}
  {:else if page === 'settings'}
    {#key seg[1]}
      <BucketSettings bucket={seg[1]} tab={route.query.tab ?? 'general'} {info} />
    {/key}
  {:else if page === 'usage'}
    <Usage />
  {:else}
    <Buckets {info} />
  {/if}
</Shell>
