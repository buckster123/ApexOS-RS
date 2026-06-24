// ApexOS PWA service worker — makes the web client installable + offline-capable.
// Network-first for the app shell (so a deploy shows up immediately when online),
// cache as the offline fallback. API + WS are never touched (network-only).
const CACHE = 'apexos-web-v1';
const SHELL = ['/', '/index.html', '/style.css', '/app.js', '/manifest.json', '/icon.svg'];

self.addEventListener('install', (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

self.addEventListener('fetch', (e) => {
  const url = new URL(e.request.url);
  // Never intercept the live agent surface — let API + WS hit the network directly.
  if (url.pathname.startsWith('/api') || url.pathname.startsWith('/ws')) return;
  if (e.request.method !== 'GET') return;

  e.respondWith(
    fetch(e.request)
      .then((res) => {
        if (res && res.ok) {
          const copy = res.clone();
          caches.open(CACHE).then((c) => c.put(e.request, copy));
        }
        return res;
      })
      .catch(() => caches.match(e.request).then((hit) => hit || caches.match('/index.html')))
  );
});
