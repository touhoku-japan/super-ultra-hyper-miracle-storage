// Hash routing (`#/path?query`): the console is served from / and /ui/, so the path part of the
// URL never changes.

function current() {
  const h = decodeURI(location.hash.slice(1) || '/');
  const [path, q = ''] = h.split('?');
  return { path: path || '/', query: Object.fromEntries(new URLSearchParams(q)) };
}

export const route = $state(current());

window.addEventListener('hashchange', () => {
  Object.assign(route, current());
});

/** `#/a/b?x=1` for `href`. Path segments are encoded. */
export function link(segments, query) {
  const path = '/' + segments.map((s) => encodeURIComponent(s)).join('/');
  const q = query
    ? Object.entries(query)
        .filter(([, v]) => v !== undefined && v !== null && v !== '')
        .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`)
        .join('&')
    : '';
  return `#${path}${q ? `?${q}` : ''}`;
}

export function go(segments, query) {
  location.hash = link(segments, query).slice(1);
}

/** The route's path segments, decoded. */
export function segments() {
  return route.path
    .split('/')
    .filter(Boolean)
    .map((s) => {
      try {
        return decodeURIComponent(s);
      } catch {
        return s;
      }
    });
}
