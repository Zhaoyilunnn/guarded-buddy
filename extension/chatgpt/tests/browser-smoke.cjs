// Optional native-browser smoke test. Requires Playwright and a Chromium executable.
const { chromium } = require(process.env.BUDDY_PLAYWRIGHT_MODULE || 'playwright');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const root = path.resolve(__dirname, '..');
(async () => {
  const server = http.createServer((req, res) => {
    const name = new URL(req.url, 'http://localhost').pathname.slice(1);
    if (!/^(popup|history)\.(html|js)$|^(ui|store|background|api)\.(js|css)$/.test(name)) { res.writeHead(404).end(); return; }
    res.setHeader('Content-Type', name.endsWith('js') ? 'text/javascript' : name.endsWith('css') ? 'text/css' : 'text/html');
    res.end(fs.readFileSync(path.join(root, name)));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  let browser;
  try {
    browser = await chromium.launch({ executablePath: process.env.BUDDY_CHROMIUM_EXECUTABLE, headless: true, args: ['--no-sandbox'] });
    const context = await browser.newContext({ viewport: { width: 420, height: 880 } });
    await context.addInitScript(() => {
      const account = 'a'.repeat(64);
      const now = Date.now();
      const body = id => ({ conversation_id: id, title: id === 'two' ? '<img src=x onerror=alert(1)>' : 'First', current_node: 'n', mapping: {}, update_time: 100, metadata: 'x'.repeat(512 * 1024) });
      if (!localStorage.getItem('fixture')) localStorage.setItem('fixture', JSON.stringify({ token: 'b'.repeat(64), port: 18242, historyStartedAt: now, status: {} }));
      const read = () => JSON.parse(localStorage.getItem('fixture'));
      const listeners = [];
      let handler;
      window.__metrics = { providerCalls: 0, details: 0, queueWrites: 0 };
      const nativePut = IDBObjectStore.prototype.put;
      IDBObjectStore.prototype.put = function(value, key) { if (this.name === 'pending') window.__metrics.queueWrites++; return nativePut.call(this, value, key); };
      window.chrome = { storage: { local: {
        async get(keys) { const data = read(); return keys === null ? data : Object.fromEntries((Array.isArray(keys) ? keys : [keys]).filter(k => k in data).map(k => [k, data[k]])); },
        async set(values) { const data = read(); const changes = {}; for (const [key, value] of Object.entries(values)) { changes[key] = { oldValue: data[key], newValue: value }; data[key] = value; } localStorage.setItem('fixture', JSON.stringify(data)); queueMicrotask(() => listeners.forEach(fn => fn(changes, 'local'))); },
        async remove(keys) { const data = read(); for (const key of Array.isArray(keys) ? keys : [keys]) delete data[key]; localStorage.setItem('fixture', JSON.stringify(data)); },
      }, onChanged: { addListener(fn) { listeners.push(fn); } } }, runtime: {
        id: 'native', getURL: p => `chrome-extension://native/${p}`, onMessage: { addListener(fn) { handler = fn; } },
        async sendMessage(message) { await window.__background; return new Promise(resolve => handler(message, { id: 'native', url: 'chrome-extension://native/popup.html' }, resolve)); },
      }, tabs: { query: async () => [{ url: 'https://chatgpt.com/c/one' }], create: async () => {} } };
      const nativeFetch = window.fetch;
      const reply = value => ({ ok: true, status: 200, json: async () => value });
      window.fetch = async (url, options) => {
        if (url.includes('chatgpt.com')) window.__metrics.providerCalls++;
        if (url.endsWith('/health')) return reply({ app: 'buddy', schema_version: 1, storage_id: 'native', storage_path: '/archive/chatgpt' });
        if (url.endsWith('/api/auth/session')) return reply({ accessToken: 'synthetic', user: { id: 'synthetic-user' } });
        if (url.includes('/conversations?')) return reply({ total: 2, items: ['one', 'two'].map(id => ({ id, update_time: 100 })) });
        if (url.includes('/conversation/')) { window.__metrics.details++; if (window.__hold) await new Promise(resolve => { window.__release = resolve; }); return reply(body(url.split('/').pop())); }
        if (url.endsWith('/v1/chatgpt')) return reply({ status: 'updated', save_ms: 0.5 });
        if (url.includes('/v1/archives?')) return reply({ items: ['one', 'two'].map(id => ({ id, account, title: body(id).title, url: `https://chatgpt.com/c/${id}`, saved_at: new Date(now).toISOString(), updated_at: 100, path: `/archive/${id}.json`, markdown_path: `/archive/${id}.md`, markdown_status: 'ready' })), total: 2, accounts: [account], warnings: [], refreshing: false });
        return nativeFetch(url, options);
      };
      window.__background = import('/background.js');
    });
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    const base = `http://127.0.0.1:${server.address().port}`;
    await page.goto(`${base}/popup.html`);
    await page.waitForFunction(() => document.querySelector('#headline').textContent === 'Manual sync');
    await page.evaluate(() => chrome.runtime.sendMessage({ type: 'health' }));
    assert.equal(await page.evaluate(() => window.__metrics.providerCalls), 0);
    assert.equal(await page.locator('#pause').count(), 0);
    assert.equal(await page.locator('#migration').count(), 0);
    await page.locator('#settingsToggle').click();
    await page.locator('#connect').click();
    await page.waitForFunction(() => document.querySelector('#setup').hidden);
    assert.equal(await page.evaluate(() => window.__metrics.providerCalls), 0, 'connecting cannot start sync');
    await page.evaluate(() => { window.__hold = true; });
    await page.locator('#current').click();
    await page.waitForFunction(() => Boolean(window.__release));
    assert.equal(await page.locator('#sync').isDisabled(), true);
    await page.locator('#stop').click();
    await page.evaluate(() => { window.__hold = false; window.__release(); });
    await page.waitForFunction(() => JSON.parse(localStorage.getItem('fixture')).status.state === 'stopped');
    const pendingKey = await page.evaluate(async () => (await import('/store.js')).get('meta', 'activeProgress'));
    assert.equal(await page.evaluate(() => window.__metrics.queueWrites), 1);
    await page.reload();
    await page.evaluate(() => chrome.runtime.sendMessage({ type: 'health' }));
    assert.equal(await page.evaluate(() => window.__metrics.providerCalls), 0, 'reload cannot continue the queued task');
    await page.waitForFunction(() => document.querySelector('#sync').textContent === 'Continue sync');
    await page.locator('#sync').click();
    await page.waitForFunction(() => JSON.parse(localStorage.getItem('fixture')).status.state === 'completed');
    assert.equal(await page.evaluate(() => window.__metrics.details), 0, 'manual continuation uses the queued snapshot');
    assert.equal(await page.evaluate(() => window.__metrics.queueWrites), 0, 'continuation does not rewrite the payload');
    assert.equal(await page.evaluate(async key => (await import('/store.js')).get('pending', key), pendingKey), undefined);
    await page.waitForFunction(() => document.querySelector('#sync').textContent === 'Sync now');
    await page.locator('#sync').click();
    await page.waitForFunction(() => JSON.parse(localStorage.getItem('fixture')).status.state === 'completed' && JSON.parse(localStorage.getItem('fixture')).status.scope === 'incremental');
    await page.setViewportSize({ width: 1100, height: 900 });
    await page.goto(`${base}/history.html`);
    await page.locator('#archives details').first().waitFor();
    assert.equal(await page.locator('#archives img').count(), 0);
    await page.locator('#runsTab').click();
    await page.locator('#runs summary').first().click();
    await page.locator('.task-result').first().waitFor();
    assert.ok((await page.locator('.task-results').first().textContent()).includes('Receiver:'));
    await page.reload();
    await page.locator('#runsTab').click();
    await page.locator('#runs summary').first().waitFor();
    assert.equal(await page.locator('#runs details').count(), 2, 'task history survives a new document/worker instance');
    await page.emulateMedia({ colorScheme: 'dark' });
    assert.deepEqual(errors, []);
    console.log('Native Chromium smoke passed: no sync on connect or reload, explicit stop/continue without rewriting snapshots, safe archive titles, paged history, timings, and persistence.');
    await context.close();
  } finally { await browser?.close(); server.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
