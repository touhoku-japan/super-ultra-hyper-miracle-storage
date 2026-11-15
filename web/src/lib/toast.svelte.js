// Short confirmations and errors in the corner.

export const toasts = $state([]);
let next = 1;

export function toast(message, kind = 'ok') {
  const id = next++;
  toasts.push({ id, message, kind });
  setTimeout(() => dismiss(id), kind === 'bad' ? 9000 : 4000);
}

export function dismiss(id) {
  const i = toasts.findIndex((t) => t.id === id);
  if (i >= 0) toasts.splice(i, 1);
}
