/* TOKEN VIEW — every trade of a contract, and the chart rebuilt from them.
   Click any token symbol on the page. The Rust core resolves the token's
   Uniswap v4 pools, backfills every Swap newest-first and tails the chain;
   this file draws what lands: candles on a canvas, the trade list below. */

const TFS = [[60, '1m'], [300, '5m'], [900, '15m'], [3600, '1h'], [14400, '4h'], [86400, '1d']];

const C = {
  addr: null, view: null, tf: 60,
  data: null, n: 0,          // Float64Array of [t,o,h,l,c,v,n] * n
  view0: 0, count: 140, follow: true,
  hover: null, drag: null,
  cv: null, g: null, ro: null,
  prog: {}, tail: '—', timer: null, tradesKey: '',
};

const fmtP = (p) => (p == null || !isFinite(p) ? '—' : p >= 1000 ? `$${NF0.format(p)}` : p >= 1 ? `$${p.toFixed(4)}`
  : p >= 0.0001 ? `$${p.toFixed(6)}` : `$${p.toPrecision(3)}`);
const shortHex = (h, a = 6, b = 4) => (h ? `${h.slice(0, a)}…${h.slice(-b)}` : '—');
const feePct = (fee) => `${(fee / 10000).toFixed(2)}%`;
const poolShort = (id) => shortHex(id, 8, 4);

/* -------------------------------------------------------------- open */

async function openToken(addr, symbol, hint) {
  const modal = $('modal'), body = $('modal-body');
  body.replaceChildren(el('div', 'empty', `resolving ${symbol || addr} on chain…`));
  modal.hidden = false;
  C.addr = addr.toLowerCase(); C.view = null; C.data = null; C.n = 0; C.follow = true; C.prog = {};
  let v;
  try { v = await invoke('token_open', { address: addr, hint: hint || null }); }
  catch (e) { body.replaceChildren(el('div', 'empty', `${symbol || addr}: ${e}`)); return; }
  if (C.addr !== addr.toLowerCase()) return;   // closed or replaced while resolving
  C.view = v;
  layout(v);
  await Promise.all([loadCandles(), loadTrades(), loadStatus()]);
}

function closeToken() {
  C.addr = null; C.view = null; C.data = null;
  if (C.ro) { C.ro.disconnect(); C.ro = null; }
  C.cv = null; C.g = null;
  clearTimeout(C.timer); C.timer = null;
}

function layout(v) {
  const body = $('modal-body');
  body.textContent = '';
  const t = v.token;

  const h = el('div', 'mhead');
  const h3 = el('h3', null, t.symbol);
  h3.append(el('span', 'dim', `  ${t.name || ''}`));
  h.append(h3);
  h.append(el('div', 'dim', null)).id = 'tk-meta';
  h.append(el('div', 'addr', t.address));
  const ls = el('div', 'mlinks');
  ls.append(link(`https://dexscreener.com/robinhood/${v.price_pool}`));
  ls.append(document.createTextNode('  '));
  ls.append(link(scanTok(t.address)));
  h.append(ls);
  body.append(h);

  const bar = el('div', 'sec tfbar');
  bar.append(document.createTextNode('CHART '));
  bar.append(el('span', 'secn', `${v.usd === 'direct' ? 'USD via USDG pool' : v.usd === 'via' ? `USD via ${v.quote.symbol}/USDG` : `in ${v.quote.symbol}, no USD route`} · price pool ${poolShort(v.price_pool)} ${feePct(v.price_pool_fee)} · ${v.pools.length} of ${v.all_pools} pools indexed`));
  const tools = el('span', 'tools');
  TFS.forEach(([s, l]) => {
    const b = el('span', `tab${s === C.tf ? ' on' : ''}`, l);
    b.dataset.tf = s;
    b.addEventListener('click', () => { C.tf = s; tools.querySelectorAll('.tab').forEach((x) => x.classList.toggle('on', x === b)); C.follow = true; loadCandles(); });
    tools.append(b);
  });
  bar.append(tools);
  body.append(bar);

  const legend = el('div', 'legend'); legend.id = 'tk-legend'; legend.textContent = ' ';
  body.append(legend);

  const cv = el('canvas', 'chart'); cv.id = 'tk-chart';
  body.append(cv);
  C.cv = cv; C.g = cv.getContext('2d', { alpha: false });
  wireChart(cv);
  C.ro = new ResizeObserver(() => draw());
  C.ro.observe(cv);

  body.append(el('div', 'prog', 'indexing…')).id = 'tk-prog';

  body.append(el('div', 'mtitle', 'TRADES')).append(el('span', 'secn', '  every swap in the indexed pools · newest first · valued at the price pool'));
  const tw = el('div', 'tw trades');
  const table = el('table', 't');
  const thead = el('thead'), tr = el('tr');
  [['time'], ['side'], ['size usd', 'n'], ['price', 'n'], [t.symbol.toLowerCase(), 'n'], ['pool'], ['tx']]
    .forEach(([l, c]) => tr.append(el('th', c || null, l)));
  thead.append(tr); table.append(thead);
  const tb = el('tbody'); tb.id = 'tk-trades'; table.append(tb);
  tw.append(table); body.append(tw);
}

/* -------------------------------------------------------------- data */

async function loadCandles() {
  if (!C.addr) return;
  const addr = C.addr;
  let buf;
  try { buf = await invoke('token_candles', { address: addr, tf: C.tf }); }
  catch (e) { return; }
  if (C.addr !== addr) return;
  const u8 = buf instanceof ArrayBuffer ? new Uint8Array(buf) : new Uint8Array(buf.buffer || buf, buf.byteOffset || 0, buf.byteLength);
  const aligned = u8.byteOffset % 8 === 0 ? u8.buffer.slice(u8.byteOffset, u8.byteOffset + u8.byteLength) : u8.slice().buffer;
  C.data = new Float64Array(aligned);
  C.n = Math.floor(C.data.length / 7);
  draw();
  headline();
}

async function loadTrades() {
  if (!C.addr) return;
  const addr = C.addr;
  let rows;
  try { rows = await invoke('token_trades', { address: addr, beforeTs: 4102444800, limit: 200 }); }
  catch (e) { return; }
  if (C.addr !== addr) return;
  const key = rows.length ? `${rows.length}:${rows[0].block}:${rows[0].log_index}:${rows[0].price}` : '0';
  if (key === C.tradesKey) return;
  C.tradesKey = key;
  const v = C.view, tb = $('tk-trades');
  if (!tb) return;
  const fees = Object.fromEntries(v.pools.map((p) => [p.id, p.fee]));
  const frag = document.createDocumentFragment();
  if (!rows.length) { frag.append(el('tr')).append(td('empty', 'no swaps indexed yet')); }
  rows.forEach((r) => {
    const tr = el('tr', `${r.side} ${tier(r.usd)}`);
    tr.append(td('d', clock(r.ts)));
    tr.append(td('side', r.side === 'buy' ? 'BUY' : 'SELL'));
    tr.append(td('n size', usd(r.usd, true)));
    tr.append(td('n d', fmtP(r.price)));
    tr.append(td('n d', tokens(r.amount)));
    const pc = td('d', `${poolShort(r.pool)} ${fees[r.pool] != null ? feePct(fees[r.pool]) : ''}`);
    if (r.pool === v.price_pool) pc.title = 'price pool';
    tr.append(pc);
    tr.append(td('u', link(scanTx(r.tx), shortHex(r.tx, 10, 4), scanTx(r.tx))));
    frag.append(tr);
  });
  tb.replaceChildren(frag);
}

async function loadStatus() {
  if (!C.addr) return;
  const addr = C.addr;
  let s;
  try { s = await invoke('token_status', { address: addr }); } catch (e) { return; }
  if (C.addr !== addr) return;
  C.status = s;
  progress();
}

function headline() {
  const v = C.view, m = $('tk-meta');
  if (!v || !m) return;
  const d = C.data, n = C.n;
  if (!n) { m.textContent = 'no trades indexed yet'; return; }
  const last = d[(n - 1) * 7 + 4];
  // 24h change: close of the first candle whose bucket is within the last 24h
  const cutoff = Date.now() / 1000 - 86400;
  let i = 0; while (i < n - 1 && d[i * 7] < cutoff) i++;
  const ref = i > 0 ? d[(i - 1) * 7 + 4] : d[1];
  const chg = ref ? ((last - ref) / ref) * 100 : null;
  let vol = 0, trades = 0; for (let k = i; k < n; k++) { vol += d[k * 7 + 5]; trades += d[k * 7 + 6]; }
  m.textContent = '';
  m.append(el('span', 'big', fmtP(last)));
  m.append(document.createTextNode('  24h '));
  m.append(colour(pct(chg), chg));
  m.append(document.createTextNode(`  vol ${usd(vol)} · ${count(trades)} trades  ·  quote ${v.quote.symbol}  ·  ${v.token.decimals} dec`));
}

function progress() {
  const v = C.view, p = $('tk-prog');
  if (!v || !p) return;
  const s = C.status || {};
  const main = C.prog[v.token.address];
  const refp = v.ref_pool ? C.prog[`ref:${v.ref_pool}`] : null;
  const pctOf = (x) => (x && x.total ? `${Math.min(100, Math.round((x.done / x.total) * 100))}%` : '…');
  const parts = [
    `${count(s.swaps || 0)} swaps indexed`,
    s.first_ts ? `history from ${stamp(s.first_ts)}` : null,
    main && !main.finished ? `scanning ${pctOf(main)} (${count(main.done)} / ${count(main.total)} blocks)` : main ? 'history complete' : null,
    v.ref_pool ? (refp && !refp.finished ? `ref ${pctOf(refp)}` : 'ref ready') : null,
    `tail: ${C.tail}`,
  ].filter(Boolean);
  p.textContent = parts.join('  ·  ');
}

// live and progress events coalesce into one refresh per ~350ms
function schedule() {
  if (C.timer || !C.addr) return;
  C.timer = setTimeout(async () => {
    C.timer = null;
    await Promise.all([loadCandles(), loadTrades(), loadStatus()]);
  }, 350);
}

/* -------------------------------------------------------------- chart */

function niceStep(raw) {
  const p = Math.pow(10, Math.floor(Math.log10(raw)));
  const m = raw / p;
  return (m < 1.5 ? 1 : m < 3 ? 2 : m < 7 ? 5 : 10) * p;
}
const TLABEL = new Intl.DateTimeFormat('en-US', { timeZone: TZ, hour: '2-digit', minute: '2-digit', hour12: false });
const DLABEL = new Intl.DateTimeFormat('en-US', { timeZone: TZ, month: 'numeric', day: 'numeric' });
const timeLabel = (t, tf) => (tf >= 86400 ? DLABEL.format(t * 1000) : tf >= 3600 ? `${DLABEL.format(t * 1000)} ${TLABEL.format(t * 1000)}` : TLABEL.format(t * 1000));

function draw() {
  const cv = C.cv, g = C.g;
  if (!cv || !g) return;
  const dpr = window.devicePixelRatio || 1;
  const W = cv.clientWidth, H = cv.clientHeight;
  if (!W || !H) return;
  if (cv.width !== Math.round(W * dpr) || cv.height !== Math.round(H * dpr)) { cv.width = Math.round(W * dpr); cv.height = Math.round(H * dpr); }
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.fillStyle = '#000'; g.fillRect(0, 0, W, H);
  g.font = '11px "JetBrains Mono", Menlo, monospace';
  g.textBaseline = 'middle';

  const d = C.data, n = C.n;
  if (!n) { g.fillStyle = '#4a4a4a'; g.textAlign = 'center'; g.fillText('no trades yet — indexing', W / 2, H / 2); return; }

  const axisW = 88, axisH = 18;
  const pw = W - axisW, ph = H - axisH;
  const volH = Math.floor(ph * 0.18), priceH = ph - volH - 6;

  C.count = Math.max(12, Math.min(C.count, Math.max(n, 12)));
  if (C.follow) C.view0 = Math.max(0, n - C.count);
  C.view0 = Math.max(0, Math.min(C.view0, Math.max(0, n - 1)));
  const i0 = C.view0, i1 = Math.min(n, i0 + C.count);
  const cw = pw / C.count;

  let lo = Infinity, hi = -Infinity, vmax = 0;
  for (let i = i0; i < i1; i++) { const b = i * 7; if (d[b + 3] < lo) lo = d[b + 3]; if (d[b + 2] > hi) hi = d[b + 2]; if (d[b + 5] > vmax) vmax = d[b + 5]; }
  if (!isFinite(lo)) { lo = 0; hi = 1; }
  if (hi === lo) { lo *= 0.99; hi *= 1.01 || 1; }
  const pad = (hi - lo) * 0.06; lo -= pad; hi += pad;
  const y = (p) => 3 + ((hi - p) / (hi - lo)) * (priceH - 6);
  const x = (i) => (i - i0) * cw + cw / 2;

  // price grid
  const step = niceStep((hi - lo) / 6);
  g.strokeStyle = '#141414'; g.lineWidth = 1; g.fillStyle = '#7a7a7a'; g.textAlign = 'left';
  for (let p = Math.ceil(lo / step) * step; p < hi; p += step) {
    const yy = Math.round(y(p)) + 0.5;
    g.beginPath(); g.moveTo(0, yy); g.lineTo(pw, yy); g.stroke();
    g.fillText(fmtP(p), pw + 6, yy);
  }

  // time grid
  const every = Math.max(1, Math.ceil(90 / cw));
  g.fillStyle = '#7a7a7a'; g.textAlign = 'center';
  for (let i = i0; i < i1; i++) {
    if (i % every) continue;
    const xx = Math.round(x(i)) + 0.5;
    g.strokeStyle = '#0e0e0e'; g.beginPath(); g.moveTo(xx, 0); g.lineTo(xx, ph); g.stroke();
    g.fillText(timeLabel(d[i * 7], C.tf), xx, ph + axisH / 2 + 1);
  }

  // volume
  const vy0 = priceH + 6 + volH;
  for (let i = i0; i < i1; i++) {
    const b = i * 7, up = d[b + 4] >= d[b + 1];
    const hgt = vmax ? (d[b + 5] / vmax) * volH : 0;
    g.fillStyle = up ? 'rgba(95,240,95,.35)' : 'rgba(255,92,92,.35)';
    const bw = Math.max(1, cw * 0.7);
    g.fillRect(x(i) - bw / 2, vy0 - hgt, bw, hgt);
  }

  // candles
  const bw = Math.max(1, Math.floor(cw * 0.7));
  for (let i = i0; i < i1; i++) {
    const b = i * 7, o = d[b + 1], h = d[b + 2], l = d[b + 3], c = d[b + 4];
    const up = c >= o, col = up ? '#5ff05f' : '#ff5c5c';
    const xx = Math.round(x(i)) + 0.5;
    g.strokeStyle = col; g.fillStyle = col; g.lineWidth = 1;
    g.beginPath(); g.moveTo(xx, y(h)); g.lineTo(xx, y(l)); g.stroke();
    const yo = y(o), yc = y(c);
    const top = Math.min(yo, yc), hgt = Math.max(1, Math.abs(yc - yo));
    if (bw <= 2) { g.fillRect(xx - 0.5, top, 1, hgt); }
    else { g.fillRect(Math.round(xx - bw / 2), Math.round(top), bw, Math.round(hgt) || 1); }
  }

  // last price
  const last = d[(n - 1) * 7 + 4], prev = n > 1 ? d[(n - 2) * 7 + 4] : last;
  const ly = Math.round(y(last)) + 0.5;
  if (ly > 0 && ly < priceH) {
    g.setLineDash([3, 3]); g.strokeStyle = last >= prev ? '#5ff05f' : '#ff5c5c';
    g.beginPath(); g.moveTo(0, ly); g.lineTo(pw, ly); g.stroke(); g.setLineDash([]);
    g.fillStyle = last >= prev ? '#5ff05f' : '#ff5c5c'; g.fillRect(pw, ly - 7, axisW, 14);
    g.fillStyle = '#000'; g.textAlign = 'left'; g.fillText(fmtP(last), pw + 6, ly);
  }

  // crosshair
  const hv = C.hover;
  if (hv) {
    const i = Math.min(i1 - 1, Math.max(i0, i0 + Math.floor(hv.x / cw)));
    const xx = Math.round(x(i)) + 0.5;
    g.strokeStyle = '#4a4a4a'; g.setLineDash([2, 3]);
    g.beginPath(); g.moveTo(xx, 0); g.lineTo(xx, ph); g.stroke();
    if (hv.y < priceH) {
      const yy = Math.round(hv.y) + 0.5;
      g.beginPath(); g.moveTo(0, yy); g.lineTo(pw, yy); g.stroke();
      const p = hi - ((yy - 3) / (priceH - 6)) * (hi - lo);
      g.setLineDash([]); g.fillStyle = '#d8d8d8'; g.fillRect(pw, yy - 7, axisW, 14);
      g.fillStyle = '#000'; g.textAlign = 'left'; g.fillText(fmtP(p), pw + 6, yy);
    }
    g.setLineDash([]);
    const b = i * 7;
    legendText(`${stamp(d[b])}  O ${fmtP(d[b + 1])}  H ${fmtP(d[b + 2])}  L ${fmtP(d[b + 3])}  C ${fmtP(d[b + 4])}  vol ${usd(d[b + 5])}  ${d[b + 6]} trades`, d[b + 4] >= d[b + 1]);
  } else {
    const b = (n - 1) * 7;
    legendText(`${stamp(d[b])}  O ${fmtP(d[b + 1])}  H ${fmtP(d[b + 2])}  L ${fmtP(d[b + 3])}  C ${fmtP(d[b + 4])}  vol ${usd(d[b + 5])}  ${d[b + 6]} trades   ·  ${n} candles, ${i1 - i0} shown  ·  wheel zoom, drag pan`, d[b + 4] >= d[b + 1]);
  }
}

function legendText(t, up) {
  const l = $('tk-legend');
  if (!l) return;
  l.textContent = t;
  l.className = `legend ${up ? 'up' : 'down'}`;
}

function wireChart(cv) {
  cv.addEventListener('wheel', (e) => {
    e.preventDefault();
    if (!C.n) return;
    const r = cv.getBoundingClientRect();
    const pw = r.width - 88;
    const mx = Math.min(pw, Math.max(0, e.clientX - r.left));
    const cw = pw / C.count;
    const anchor = C.view0 + mx / cw;
    if (Math.abs(e.deltaX) > Math.abs(e.deltaY)) {
      // horizontal: pan
      C.view0 += (e.deltaX / cw);
      C.follow = C.view0 + C.count >= C.n;
    } else {
      const f = e.deltaY > 0 ? 1.15 : 1 / 1.15;
      C.count = Math.max(12, Math.min(Math.max(C.n, 12), Math.round(C.count * f)));
      const cw2 = pw / C.count;
      C.view0 = anchor - mx / cw2;
      C.follow = C.view0 + C.count >= C.n - 0.5;
    }
    C.view0 = Math.round(C.view0);
    draw();
  }, { passive: false });
  cv.addEventListener('pointerdown', (e) => { C.drag = { x: e.clientX, v0: C.view0 }; cv.setPointerCapture(e.pointerId); });
  cv.addEventListener('pointermove', (e) => {
    const r = cv.getBoundingClientRect();
    if (C.drag) {
      const cw = (r.width - 88) / C.count;
      C.view0 = Math.round(C.drag.v0 - (e.clientX - C.drag.x) / cw);
      C.follow = C.view0 + C.count >= C.n;
    }
    C.hover = { x: e.clientX - r.left, y: e.clientY - r.top };
    draw();
  });
  cv.addEventListener('pointerup', () => { C.drag = null; });
  cv.addEventListener('pointerleave', () => { C.hover = null; C.drag = null; draw(); });
  cv.addEventListener('dblclick', () => { C.follow = true; C.count = 140; draw(); });
}

/* ---------------------------------------------------------------- rpc */

async function openRpc() {
  const modal = $('modal'), body = $('modal-body');
  closeToken();
  let cur = { http: '', ws: '', paid: false };
  try { cur = await invoke('rpc_get'); } catch (e) { /* defaults */ }
  body.textContent = '';
  const h = el('div', 'mhead');
  h.append(el('h3', null, 'RPC NODE'));
  h.append(el('div', 'dim', 'the chain node the indexer reads from · must serve robinhood chain 4663 · websocket is optional: with it the live tail is a subscription, without it the tape is polled every 400ms'));
  body.append(h);
  const f = el('div', 'rpcform');
  const mk = (label, id, val, ph) => {
    const row = el('div', 'rpcrow');
    row.append(el('span', 'k2', label));
    const i = el('input'); i.id = id; i.value = val || ''; i.placeholder = ph; i.spellcheck = false; i.autocomplete = 'off';
    row.append(i); f.append(row);
  };
  mk('http', 'rpc-http', cur.http, 'https://…');
  mk('ws', 'rpc-ws', cur.ws, 'wss://… (optional)');
  const row = el('div', 'rpcrow');
  const save = el('button', 'btn', 'save + test');
  const msg = el('span', 'dim', cur.paid ? 'using your node' : 'using the public node (rate limited, no websocket)');
  row.append(save, msg); f.append(row);
  body.append(f);
  modal.hidden = false;
  $('rpc-http').focus();
  save.addEventListener('click', async () => {
    msg.textContent = 'testing…';
    try {
      const r = await invoke('rpc_set', { http: $('rpc-http').value, ws: $('rpc-ws').value });
      msg.textContent = `ok · chain ${r.chain_id} · websocket ${r.ws_ok ? 'subscribed' : ($('rpc-ws').value.trim() ? 'failed, will poll' : 'not set, will poll')}`;
      $('k-rpc').textContent = r.paid ? 'own' : 'public';
    } catch (e) { msg.textContent = `failed: ${e}`; }
  });
}

/* --------------------------------------------------------------- wire */

(function wireChartModule() {
  // token symbols anywhere on the page open the token view
  document.addEventListener('click', (e) => {
    const s = e.target.closest ? e.target.closest('.sym[data-token]') : null;
    if (!s) return;
    if (e.target.closest('a')) return;
    openToken(s.dataset.token, s.textContent, s.dataset.pair || null);
  });
  // when the modal hides (esc, close, backdrop), the token view is gone
  new MutationObserver(() => { if ($('modal').hidden) closeToken(); })
    .observe($('modal'), { attributes: true, attributeFilter: ['hidden'] });

  TAURI.event.listen('idx', (ev) => {
    const p = ev.payload;
    C.prog[p.key] = p;
    if (!C.view) return;
    if (p.key === C.view.token.address || p.key === `ref:${C.view.ref_pool}`) { progress(); schedule(); }
  });
  TAURI.event.listen('swap', (ev) => {
    if (!C.view) return;
    const mine = new Set(C.view.pools.map((p) => p.id)); if (C.view.ref_pool) mine.add(C.view.ref_pool);
    if (ev.payload.some((s) => mine.has(s.pool))) schedule();
  });
  TAURI.event.listen('rpc', (ev) => { C.tail = ev.payload.mode; progress(); });
  invoke('rpc_get').then((r) => { $('k-rpc').textContent = r.paid ? 'own' : 'public'; }).catch(() => {});
})();
