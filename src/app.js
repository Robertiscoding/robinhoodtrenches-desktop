/* ROBINHOOD TRENCHES — front end, desktop build.
   Same page as robinhoodtrenches.com. The wire is native: REST calls and the
   live socket go through the Rust core (pooled HTTP/2, no CORS, no browser
   cache); the page only parses JSON and paints. */

const TAURI = window.__TAURI__;
const invoke = TAURI.core.invoke;

const S = {
  window: '24h',
  stocks: false,
  sound: true,           // audible only after the webview lets us: first click or key
  unlocked: false,
  session: { usd: 0, fills: 0, buys: 0, sells: 0, since: Date.now() },
  follow: true,
  activeOnly: true,
  sort: 'realized_pnl',
  filter: '',
  tab: 'tokens',
  lastId: 0,
  fills: [],
  traders: [],
};
// The tape is the page's own height, so every new fill re-lays-out the whole
// table; 150 rows is still more than a screen and a third of the layout cost.
const MAX_ROWS = 150;
// counters a viewer (or a test) can read to see what the page is doing
window.__perf = { rebuilt: 0, skipped: 0, flashes: 0 };
const BIG_USD = 5000;

const $ = (id) => document.getElementById(id);
const el = (tag, cls, text) => {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text != null) n.textContent = text;
  return n;
};

/* ----------------------------------------------------------- formatting */

// Intl objects are expensive to construct and the tape formats thousands of
// numbers and timestamps a minute; build each formatter once and reuse it.
const NF0 = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 });
const NF2 = new Intl.NumberFormat(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 });

function usd(v, force) {
  if (v == null || Number.isNaN(v)) return '—';
  const a = Math.abs(v);
  if (a >= 1e9) return `$${(v / 1e9).toFixed(2)}B`;
  if (a >= 1e6) return `$${(v / 1e6).toFixed(2)}M`;
  if (a >= 1e4 && !force) return `$${NF0.format(Math.round(v))}`;
  return `$${NF2.format(v)}`;
}
const signed = (v) => (v == null ? '—' : (v >= 0 ? '+' : '-') + usd(Math.abs(v)));
const cls = (v) => (v == null ? 'flat' : v > 0.005 ? 'up' : v < -0.005 ? 'down' : 'flat');
const pct = (v, dp = 1) => (v == null ? '—' : `${v >= 0 ? '+' : ''}${v.toFixed(dp)}%`);
const rate = (v) => (v == null ? '—' : `${Math.round(v * 100)}%`);

function count(v) {
  if (v == null) return '—';
  if (v >= 1e6) return `${(v / 1e6).toFixed(1)}M`;
  if (v >= 1e4) return `${(v / 1e3).toFixed(1)}K`;
  return NF0.format(v);
}
function tokens(v) {
  if (v == null) return '—';
  if (v >= 1e9) return `${(v / 1e9).toFixed(2)}B`;
  if (v >= 1e6) return `${(v / 1e6).toFixed(2)}M`;
  if (v >= 1e3) return `${(v / 1e3).toFixed(1)}K`;
  if (v >= 1) return v.toFixed(2);
  return v.toPrecision(3);
}
function price(v) {
  if (v == null) return '—';
  if (v >= 1) return `$${v.toFixed(4)}`;
  if (v >= 0.0001) return `$${v.toFixed(6)}`;
  return `$${v.toExponential(2)}`;
}
// Every timestamp on the page is New York time, whoever is looking at it.
const TZ = 'America/New_York';
const CLOCK_F = new Intl.DateTimeFormat('en-US', {
  timeZone: TZ, hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false });
const STAMP_F = new Intl.DateTimeFormat('en-US', {
  timeZone: TZ, month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false });
const clock = (ts) => CLOCK_F.format(ts * 1000);
const stamp = (ts) => STAMP_F.format(ts * 1000);

function held(sec) {
  if (sec == null) return '—';
  if (sec < 60) return `${Math.round(sec)}s`;
  if (sec < 3600) return `${Math.floor(sec / 60)}m${String(Math.round(sec % 60)).padStart(2, '0')}s`;
  if (sec < 86400) return `${(sec / 3600).toFixed(1)}h`;
  return `${(sec / 86400).toFixed(1)}d`;
}

const dexUrl = (t) => `https://dexscreener.com/robinhood/${t}`;
const scanTok = (t) => `https://robinhoodchain.blockscout.com/token/${t}`;
const scanAddr = (a) => `https://robinhoodchain.blockscout.com/address/${a}`;
const scanTx = (t) => `https://robinhoodchain.blockscout.com/tx/${t}`;
const fomoUrl = (h) => `https://fomo.family/profile/${h}`;

function link(href, text, title, short) {
  const a = el('a', null);
  if (short) {
    // the full URL is the desktop rendering; phones show the short label
    a.append(el('span', 'full', text == null ? href : text));
    a.append(el('span', 'short', short));
  } else {
    a.textContent = text == null ? href : text;
  }
  a.href = href; a.target = '_blank'; a.rel = 'noopener noreferrer';
  if (title) a.title = title;
  return a;
}
function who(handle) {
  const s = el('span', 'who', handle);
  s.addEventListener('click', () => openTrader(handle));
  return s;
}
function td(cls_, ...kids) {
  const c = el('td', cls_ || null);
  kids.forEach((k) => c.append(k instanceof Node ? k : document.createTextNode(k == null ? '—' : String(k))));
  return c;
}
const colour = (text, v) => el('span', cls(v), text);
// a token symbol that opens the token view: carries the contract and, when
// the API gave one, the dexscreener pair (a uniswap v4 poolId) as the price hint
function sym(text, token, pairUrl) {
  const s = el('span', 'sym', text);
  if (token) { s.dataset.token = token; s.title = 'chart + every trade'; }
  if (pairUrl) { const id = String(pairUrl).split('/').pop(); if (/^0x[0-9a-f]{64}$/i.test(id)) s.dataset.pair = id; }
  return s;
}

/* ---------------------------------------------------------------- api */

// One round trip to the Rust core, which holds a warm HTTP/2 connection to
// the site. The body comes back as text and is parsed exactly once, here.
async function api(path, params = {}) {
  let text;
  try { text = await invoke('api', { path, query: String(new URLSearchParams(params)) }); }
  catch (e) { throw new Error(typeof e === 'string' ? e : (e && e.message) || `${path} failed`); }
  return JSON.parse(text);
}
const P = () => ({ window: S.window, stocks: S.stocks ? 'true' : 'false' });

/* ------------------------------------------------------------ odometer */

// Big numbers roll to their new value instead of snapping, so a change reads
// as money moving rather than a page refreshing.
function roll(id, target, fmt) {
  const node = $(id);
  const from = parseFloat(node.dataset.n || '0') || 0;
  node.dataset.n = String(target);
  if (target == null || Number.isNaN(target)) { node.textContent = '—'; return; }
  if (!from || Math.abs(target - from) < 1e-9) { node.textContent = fmt(target); return; }
  const t0 = performance.now(), ms = 700;
  const step = (now) => {
    const k = Math.min(1, (now - t0) / ms);
    const e = 1 - Math.pow(1 - k, 3);
    node.textContent = fmt(from + (target - from) * e);
    if (k < 1) requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
}

/* --------------------------------------------------------------- sound */

let ac = null;
let lastBlip = 0;

// The webview refuses to make a sound until the visitor has clicked or typed.
// The first gesture anywhere on the page creates and resumes the context;
// until then the bar says "on (click)" so a silent boot does not read as broken.
function unlockAudio() {
  if (S.unlocked) return;
  try {
    ac = ac || new (window.AudioContext || window.webkitAudioContext)();
    ac.resume().then(() => { S.unlocked = true; $('k-sound').textContent = S.sound ? 'on' : 'off'; });
  } catch (e) { /* no audio here */ }
}

function blip(size, side) {
  if (!S.sound || !S.unlocked) return;
  // during the replay bursts dozens of fills land within a second; one blip
  // per 60ms turns that into a rattle instead of a smear
  const now = performance.now();
  if (now - lastBlip < 60) return;
  lastBlip = now;
  try {
    ac = ac || new (window.AudioContext || window.webkitAudioContext)();
    const t = ac.currentTime, osc = ac.createOscillator(), g = ac.createGain();
    const w = Math.min(1, Math.log10(Math.max(size, 10)) / 5);
    osc.type = side === 'buy' ? 'triangle' : 'sawtooth';
    osc.frequency.setValueAtTime(side === 'buy' ? 520 - w * 240 : 340 - w * 150, t);
    osc.frequency.exponentialRampToValueAtTime(side === 'buy' ? 880 : 170, t + 0.13);
    g.gain.setValueAtTime(0.0001, t);
    g.gain.exponentialRampToValueAtTime(0.02 + w * 0.05, t + 0.01);
    g.gain.exponentialRampToValueAtTime(0.0001, t + 0.3);
    osc.connect(g).connect(ac.destination);
    osc.start(t); osc.stop(t + 0.32);
  } catch (e) { /* audio is optional */ }
}

function shout(f) {
  const box = $('big');
  $('big-text').textContent =
    `>> ${f.handle} ${f.side.toUpperCase()} ${usd(f.usd)} ${f.symbol || ''}`
    + (window.innerWidth > 760 ? `  ${dexUrl(f.token)}` : '');
  box.className = `callout ${f.side}`;
  box.hidden = false;
  box.style.animation = 'none'; void box.offsetWidth; box.style.animation = '';
  clearTimeout(shout._t);
  shout._t = setTimeout(() => { box.hidden = true; }, 2400);
}

/* ----------------------------------------------------------- 01 the tape */

const tier = (u) => (u == null ? '' : u >= 5000 ? 'sz4' : u >= 1000 ? 'sz3' : u >= 250 ? 'sz2' : '');

function tapeRow(f, fresh) {
  // no flash while the window is hidden -- animations do not run there and
  // would pile up to play all at once on return; and the class comes off after
  // it has played so finished animations are not carried around forever
  const flash = fresh && !document.hidden && !(TOUCH && S.replaying);
  const tr = el('tr', `${f.side} ${tier(f.usd)}${flash ? ' new' : ''}`);
  if (flash) { window.__perf.flashes++; setTimeout(() => tr.classList.remove('new'), 1300); }
  tr.append(td('d', clock(f.ts)));
  tr.append(td('side', f.side === 'buy' ? 'BUY' : 'SELL'));

  const tok = el('td');
  tok.append(sym(f.symbol || f.token.slice(0, 10), f.token, f.pair_url));
  if (f.new_position) tok.append(el('span', 'newpos', ' FIRST BUY'));
  if (f.is_stock) tok.append(el('span', 'stk', ' STOCK'));
  // quiet labels for fills the reader should not take at face value
  const flags = f.flags || [];
  if (flags.length) {
    tok.append(el('span', 'flag', ' · ' + flags.slice(0, 2).join(' · ')));
    // fade only what nobody paid for; a real sale of a spam-named token is still real money
    if (flags.some((x) => x.startsWith('not a real'))) tr.classList.add('sus');
  }
  tr.append(tok);

  // A tilde means the size is a price-feed estimate, not the USDG actually paid.
  const exact = f.priced == null || f.priced === 'cash_leg';
  const size = td('n size', f.usd == null ? '—' : (exact ? '' : '~') + usd(f.usd, true));
  if (!exact) size.title = 'estimated from a price feed — this transaction had no readable cash leg';
  tr.append(size);

  tr.append(td('n d m-hide', price(f.price)));
  tr.append(td('n d m-hide', tokens(f.amount)));
  tr.append(td(null, who(f.handle)));
  tr.append(td('n d m-hide', count(f.followers)));
  tr.append(td('u', link(fomoUrl(f.handle), null, null, 'fomo')));
  tr.append(td('u', link(dexUrl(f.token), null, null, 'dex')));
  tr.append(td('u m-hide', link(scanTx(f.tx), `${f.tx.slice(0, 10)}…${f.tx.slice(-4)}`, scanTx(f.tx))));
  return tr;
}

function passes(f) {
  if (!S.stocks && f.is_stock) return false;
  if (!S.filter) return true;
  const q = S.filter.toLowerCase();
  return (f.handle || '').toLowerCase().includes(q) || (f.symbol || '').toLowerCase().includes(q);
}

function drawTape() {
  const body = $('tape');
  const rows = S.fills.filter(passes).slice(0, MAX_ROWS);
  const frag = document.createDocumentFragment();
  if (!rows.length) frag.append(el('tr')).append(td('empty', 'nothing in view'));
  else rows.forEach((f) => frag.append(tapeRow(f, false)));
  body.replaceChildren(frag);
}

function drawSession() {
  const z = S.session;
  const secs = Math.floor((Date.now() - z.since) / 1000);
  $('session').textContent =
    `session ${usd(z.usd)} · ${z.fills} fills · ${z.buys}B ${z.sells}S · ${held(secs)}`;
}

function pushFills(fills) {
  if (S.replaying) { S.pending.push(...fills); return; }
  // what has happened since this page was opened -- live only, never history
  let moved = false;
  for (const f of fills) {
    if (f.id <= S.lastId || (!S.stocks && f.is_stock)) continue;
    S.session.usd += f.usd || 0;
    S.session.fills += 1;
    S.session[f.side === 'buy' ? 'buys' : 'sells'] += 1;
    moved = true;
  }
  if (moved) {
    drawSession();
    const node = $('session');
    node.classList.remove('tick'); void node.offsetWidth; node.classList.add('tick');
  }
  const body = $('tape');
  const wrap = body.closest('.tw');
  const atTop = wrap.scrollTop < 40;
  fills.sort((a, b) => a.id - b.id);
  // a block can land several fills at once; build them off-document and
  // insert with one mutation so the table lays out once, not once per row
  const frag = document.createDocumentFragment();
  for (const f of fills) {
    if (f.id <= S.lastId) continue;
    S.lastId = f.id;
    S.fills.unshift(f);
    if (!passes(f)) continue;
    frag.prepend(tapeRow(f, true));
    const sus = (f.flags || []).length > 0;      // a fake $50K "buy" must not flash across the top
    if (!sus && f.usd != null && f.usd >= BIG_USD) { shout(f); blip(f.usd, f.side); }
    else if (!sus && f.usd != null && f.usd >= 250) blip(f.usd, f.side);
  }
  if (frag.childNodes.length) body.prepend(frag);
  while (S.fills.length > MAX_ROWS * 2) S.fills.pop();
  while (body.children.length > MAX_ROWS) body.removeChild(body.lastChild);
  if (S.follow && atTop) wrap.scrollTop = 0;
}

/* ------------------------------------------------------------- readout */

let LAST = { status: null, o: null };

function meter(perMin) {
  const n = Math.min(10, Math.round(perMin / 2));
  return '▓'.repeat(n) + '<i>' + '░'.repeat(10 - n) + '</i>';
}

function esc(x) { return String(x).replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c])); }

function drawRo() {
  if (S.booting) return;
  const st = LAST.status, o = LAST.o;
  const w = S.window;
  const l1 = st ? `<span class="h">chain</span> robinhood/4663  <span class="h">wallets</span> ${st.wallets}  `
    + `<span class="h">block</span> ${NF0.format(st.last_block || 0)}  <span class="h">feed</span> ${esc(st.source || '—')}  `
    + `<span class="h">lag</span> ${st.lag_seconds == null ? '—' : st.lag_seconds + 's'}  `
    + (st.latency ? `<span class="h">block→tape</span> ${st.latency.median}s <span class="d">(p90 ${st.latency.p90}s)</span>  ` : '')
    + `<span class="h">fills indexed</span> ${NF0.format(st.trades || 0)}  `
    + `<span class="${S.link || 'warn'}">${esc(S.linkText || 'connecting')}</span>` : 'connecting…';
  if (!o) { $('ro').innerHTML = l1; return; }
  const perMin = (o.last_5m.buys + o.last_5m.sells) / 5;
  const bw = o.biggest_win, bb = o.biggest_buy;
  const l2 = `<span class="h">${w.toUpperCase()}</span>  <span class="h">traded</span> <span class="big">${usd(o.volume)}</span>  `
    + `${count(o.buys)} buys / ${count(o.sells)} sells  ${o.active_traders} wallets  ${count(o.tokens)} tokens  `
    + `<span class="h">win</span> <span class="${o.win_rate >= 0.5 ? '' : 'neg'}">${rate(o.win_rate)}</span> <span class="d">(${count(o.closed_trades)} closed)</span>`;
  const l3 = `     <span class="h">biggest win</span> ${bw ? `<span class="big">+${usd(bw.pnl_usd)}</span> ${esc(bw.handle)} ${esc(bw.symbol || '?')} ${pct(bw.pct, 0)} held ${held(bw.closed_ts - bw.opened_ts)}` : '<span class="d">none yet</span>'}`
    + `   <span class="h">biggest buy</span> ${bb ? `<span class="big">${usd(bb.usd)}</span> ${esc(bb.handle)} ${esc(bb.symbol || '?')} ${clock(bb.ts)}` : '—'}`;
  const l4 = `     <span class="h">activity</span> <span class="meter">${meter(perMin)}</span> ${perMin.toFixed(1)} fills/min  `
    + `${o.last_5m.buys}B ${o.last_5m.sells}S  ${usd(o.last_5m.volume)} <span class="d">last 5 min</span>`;
  $('ro').innerHTML = [l1, l2, l3, l4].join('\n');
}

function drawStats(o) { LAST.o = o; drawRo(); }

/* --------------------------------------------------- 02 closed trades */

const LAST_BODY = {};
function unchanged(key, rows) {
  // Rebuilding a few hundred rows every 15s is a hitch on a slower machine,
  // and most refreshes bring back exactly what was there. Compare first.
  const sig = JSON.stringify(rows);
  if (LAST_BODY[key] === sig) { window.__perf.skipped++; return true; }
  LAST_BODY[key] = sig; window.__perf.rebuilt++;
  return false;
}

async function drawClosed() {
  const body = $('closed');
  const rows = await api('/closed', { ...P(), limit: 80 });   // the box shows ~14; 80 is plenty to scroll
  if (unchanged('closed', rows)) return;
  const frag = document.createDocumentFragment();
  if (!rows.length) {
    const tr = el('tr'); tr.append(td('empty', 'no positions closed in this window')); body.replaceChildren(tr); return;
  }
  rows.forEach((r) => {
    const tr = el('tr', r.pnl_usd >= 0 ? 'buy' : 'sell');
    tr.append(td('d', stamp(r.closed_ts)));
    tr.append(td(null, who(r.handle)));
    tr.append(td('n d m-hide', count(r.followers)));
    const tok = el('td');
    tok.append(sym(r.symbol || '?', r.token));
    if (r.is_stock) tok.append(el('span', 'stk', ' STOCK'));
    tr.append(tok);
    tr.append(td('n d m-hide', usd(r.cost_sold)));
    tr.append(td('n d m-hide', usd(r.proceeds_usd)));
    tr.append(td('n', colour(signed(r.pnl_usd), r.pnl_usd)));
    tr.append(td('n', colour(pct(r.pnl_pct), r.pnl_pct)));
    tr.append(td('n d', held(r.hold_seconds)));
    tr.append(td('n d m-hide', `${r.buys}B/${r.sells}S`));
    tr.append(td('u', link(fomoUrl(r.handle), null, null, 'fomo')));
    tr.append(td('u', link(dexUrl(r.token), null, null, 'dex')));
    frag.append(tr);
  });
  body.replaceChildren(frag);
}

/* -------------------------------------------------------- 03 traders */

function drawBoard() {
  const body = $('board');
  let rows = S.traders.slice();
  if (S.activeOnly) rows = rows.filter((t) => t.fills > 0);
  if (unchanged('board:' + S.sort + ':' + S.activeOnly, rows)) return;
  const frag = document.createDocumentFragment();
  rows.sort((a, b) => {
    const x = a[S.sort], y = b[S.sort];
    if (x == null && y == null) return 0;
    if (x == null) return 1;
    if (y == null) return -1;
    return y - x;
  });
  if (!rows.length) {
    const tr = el('tr');
    tr.append(td('empty', S.activeOnly
      ? 'no tracked wallet traded in this window — widen it or turn off TRADED THIS WINDOW'
      : 'no wallets loaded'));
    body.replaceChildren(tr); return;
  }
  rows.forEach((t, i) => {
    const tr = el('tr');
    tr.append(td('n d', i + 1));
    tr.append(td(null, who(t.handle)));
    tr.append(td('n d m-hide', count(t.followers)));
    tr.append(td('n d m-hide', count(t.fills)));
    tr.append(td('n d m-hide', usd(t.volume)));
    tr.append(td('n', colour(signed(t.realized_pnl), t.realized_pnl)));
    tr.append(td('n', colour(signed(t.unrealized_pnl), t.unrealized_pnl)));
    tr.append(td('n', colour(signed(t.net_pnl), t.net_pnl)));
    tr.append(td('n d', t.closed_trades ? rate(t.win_rate) : '—'));
    tr.append(td('n d', t.closed_trades || '—'));
    tr.append(td('n m-hide', t.best_trade == null ? '—' : colour(signed(t.best_trade), t.best_trade)));
    tr.append(td('n m-hide', t.worst_trade == null ? '—' : colour(signed(t.worst_trade), t.worst_trade)));
    tr.append(td('d m-hide', t.address));
    tr.append(td('u', link(t.profile_url || fomoUrl(t.handle), null, null, 'fomo')));
    tr.append(td('u m-hide', link(scanAddr(t.address), null, null, 'scan')));
    frag.append(tr);
  });
  body.replaceChildren(frag);
}

/* ----------------------------------------------------------- 04 more */

function table(heads, rows) {
  const t = el('table', 't');
  const thead = el('thead'), htr = el('tr');
  heads.forEach((h) => htr.append(el('th', h.n ? 'n' : null, h.label)));
  thead.append(htr); t.append(thead);
  const tb = el('tbody');
  rows.forEach((cells) => {
    const tr = el('tr');
    cells.forEach((c, i) => tr.append(td(heads[i].n ? 'n' : null,
      ...(Array.isArray(c) ? c : [c]))));
    tb.append(tr);
  });
  t.append(tb);
  return t;
}

async function drawTokens() {
  const pane = $('tab-tokens');
  const rows = await api('/tokens', { ...P(), limit: 60 });
  if (unchanged('tokens', rows)) return;
  if (!rows.length) { pane.replaceChildren(el('div', 'empty', 'nothing bought in this window')); return; }
  pane.replaceChildren(table(
    [{ label: 'TOKEN' }, { label: 'WALLETS THAT BOUGHT', n: true }, { label: 'STILL HOLDING', n: true },
     { label: 'SPENT', n: true }, { label: 'TOOK OUT', n: true }, { label: 'NET INTO IT', n: true },
     { label: 'FIRST IN' }, { label: 'PRICE SINCE FIRST BUY', n: true }, { label: '24H', n: true },
     { label: 'POOL LIQUIDITY', n: true }, { label: 'dexscreener' }],
    rows.map((r) => [
      [sym(r.symbol || '?', r.token, r.pair_url), r.is_stock ? el('span', 'stk', ' STOCK') : ''],
      count(r.buyers), count(r.holders), usd(r.usd_in), usd(r.usd_out),
      colour(signed(r.net_usd), r.net_usd),
      r.first_buyer ? who(r.first_buyer.handle) : '—',
      colour(pct(r.since_first_buy_pct), r.since_first_buy_pct),
      colour(pct(r.change24), r.change24), usd(r.liquidity),
      link(dexUrl(r.token)),
    ])));
}

async function drawFlow() {
  const pane = $('tab-flow');
  const rows = await api('/flow', { ...P(), limit: 40 });
  if (unchanged('flow', rows)) return;
  if (!rows.length) { pane.replaceChildren(el('div', 'empty', 'no chains in this window')); return; }
  pane.replaceChildren(table(
    [{ label: 'TOKEN' }, { label: 'BOUGHT FIRST BY' }, { label: 'THEIR SIZE', n: true },
     { label: 'PILED IN AFTER', n: true }, { label: 'ORDER THEY BOUGHT' },
     { label: 'PRICE SINCE FIRST BUY', n: true }, { label: 'TOTAL SPENT', n: true }],
    rows.map((r) => {
      const chain = el('span', 'chain');
      chain.append(el('span', 'lead', r.lead.handle));
      r.followers.slice(0, 6).forEach((f) => {
        chain.append(el('span', 'ar', ' → '));
        chain.append(document.createTextNode(`${f.handle} +${held(f.lag_seconds)}`));
      });
      if (r.followers.length > 6) chain.append(el('span', 'ar', ` +${r.followers.length - 6} more`));
      return [
        [sym(r.symbol || '?', r.token, r.pair_url), ' ', link(dexUrl(r.token))],
        who(r.lead.handle), usd(r.lead.usd), count(r.follower_count), chain,
        colour(pct(r.since_lead_pct), r.since_lead_pct), usd(r.total_usd),
      ];
    })));
}

async function drawRadar() {
  const pane = $('tab-radar');
  const rows = await api('/radar', { minutes: 120, limit: 40 });
  if (unchanged('radar', rows)) return;
  if (!rows.length) { pane.replaceChildren(el('div', 'empty', 'nothing fresh right now')); return; }
  const now = Math.floor(Date.now() / 1000);
  pane.replaceChildren(table(
    [{ label: 'TOKEN' }, { label: 'FIRST TRACKED BUY', n: true }, { label: 'BY' },
     { label: 'POOL AGE WHEN BOUGHT', n: true }, { label: 'WALLETS IN', n: true },
     { label: 'SPENT', n: true }, { label: '24H', n: true }, { label: 'POOL LIQUIDITY', n: true },
     { label: 'CHART' }],
    rows.map((r) => [
      [sym(r.symbol || '?', r.token, r.pair_url), r.is_stock ? el('span', 'stk', ' STOCK') : ''],
      `${held(now - r.first_ts)} ago`,
      r.first_buyer ? who(r.first_buyer.handle) : '—',
      r.age_at_first_buy != null ? held(r.age_at_first_buy) : 'pool not indexed yet',
      count(r.buyers), usd(r.usd_in), colour(pct(r.change24), r.change24), usd(r.liquidity),
      link(dexUrl(r.token)),
    ])));
}

const TABS = { tokens: drawTokens, flow: drawFlow, radar: drawRadar };
async function drawTab() {
  try { await TABS[S.tab](); }
  catch (e) { $(`tab-${S.tab}`).replaceChildren(el('div', 'empty', `failed: ${e.message}`)); }
}

/* ------------------------------------------------------- trader modal */

function spark(points) {
  const w = 900, h = 88, pad = 4;
  const mk = (tag, at) => { const n = document.createElementNS('http://www.w3.org/2000/svg', tag);
    Object.entries(at).forEach(([k, v]) => n.setAttribute(k, v)); return n; };
  const svg = mk('svg', { viewBox: `0 0 ${w} ${h}`, class: 'curve', preserveAspectRatio: 'none' });
  if (points.length < 2) return svg;
  const xs = points.map((p) => p.ts), ys = points.map((p) => p.pnl);
  const x0 = Math.min(...xs), x1 = Math.max(...xs);
  const y0 = Math.min(0, ...ys), y1 = Math.max(0, ...ys);
  const sx = (v) => pad + ((v - x0) / (x1 - x0 || 1)) * (w - pad * 2);
  const sy = (v) => h - pad - ((v - y0) / ((y1 - y0) || 1)) * (h - pad * 2);
  svg.append(mk('line', { x1: 0, x2: w, y1: sy(0), y2: sy(0), stroke: '#35424f', 'stroke-dasharray': '3 4' }));
  const d = points.map((p, i) => `${i ? 'L' : 'M'}${sx(p.ts).toFixed(1)},${sy(p.pnl).toFixed(1)}`).join(' ');
  const c = ys[ys.length - 1] >= 0 ? '#24ff8b' : '#ff3355';
  svg.append(mk('path', { d: `${d} L${sx(x1)},${sy(y0)} L${sx(x0)},${sy(y0)} Z`, fill: c, opacity: .10 }));
  svg.append(mk('path', { d, fill: 'none', stroke: c, 'stroke-width': 1.5 }));
  return svg;
}

function cell(k, v, colourCls) {
  const c = el('div', 'c');
  c.append(el('div', 'k', k));
  c.append(el('div', `v ${colourCls || ''}`, v));
  return c;
}

async function openTrader(handle) {
  const modal = $('modal'), body = $('modal-body');
  body.replaceChildren(el('div', 'empty', `loading ${handle}…`));
  modal.hidden = false;
  let t;
  try { t = await api(`/trader/${encodeURIComponent(handle)}`, P()); }
  catch (e) { body.replaceChildren(el('div', 'empty', `no data for ${handle}`)); return; }

  body.textContent = '';
  const h = el('div', 'mhead');
  h.append(el('h3', null, t.handle));
  h.append(el('div', 'dim',
    `${count(t.followers)} followers · joined ${t.joined || '?'} · `
    + `last ${Math.abs(t.streak)} closed trades ${t.streak > 0 ? 'all won' : t.streak < 0 ? 'all lost' : '—'}`));
  h.append(el('div', 'addr', t.address));
  const ls = el('div', 'mlinks');
  ls.append(link(t.profile_url || fomoUrl(t.handle)));
  ls.append(link(scanAddr(t.address)));
  if (t.solana_address) ls.append(link(`https://solscan.io/account/${t.solana_address}`));
  h.append(ls);
  body.append(h);

  const st = t.stats, g = el('div', 'mgrid');
  g.append(cell(`P/L ON SELLS ${S.window.toUpperCase()}`, signed(st.realized_pnl), cls(st.realized_pnl)));
  g.append(cell('P/L ON OPEN BAGS', signed(st.unrealized_pnl), cls(st.unrealized_pnl)));
  g.append(cell('TOTAL P/L', signed(st.net_pnl), cls(st.net_pnl)));
  g.append(cell('WON', rate(st.win_rate)));
  g.append(cell('POSITIONS FULLY CLOSED', String(st.closed_trades)));
  g.append(cell('WON $ PER $1 LOST', st.profit_factor == null ? '—' : st.profit_factor.toFixed(2)));
  g.append(cell('BEST TRADE', signed(st.best_trade), 'up'));
  g.append(cell('WORST TRADE', signed(st.worst_trade), 'down'));
  g.append(cell('AVG TIME HELD', held(st.avg_hold_seconds)));
  g.append(cell('BAGS OPEN NOW', String(st.open_bags)));
  body.append(g);

  if ((t.curve || []).length > 1) {
    body.append(el('div', 'mtitle', `P/L ON SELLS, RUNNING TOTAL · ${S.window.toUpperCase()}`));
    body.append(spark(t.curve));
  }

  body.append(el('div', 'mtitle', `OPEN BAGS — ${t.bags.length} · marked live`));
  body.append(t.bags.length ? table(
    [{ label: 'TOKEN' }, { label: 'TOKENS HELD', n: true }, { label: 'PAID', n: true },
     { label: 'WORTH NOW', n: true }, { label: 'UP/DOWN', n: true }, { label: '%', n: true },
     { label: 'HELD FOR', n: true }, { label: 'CHART' }],
    t.bags.map((b) => [
      [sym(b.symbol || '?', b.token), b.is_stock ? el('span', 'stk', ' STOCK') : ''],
      tokens(b.amount),
      b.priced ? usd(b.cost_usd) : el('span', 'dim', 'unknown'),
      b.priced ? usd(b.value) : el('span', 'dim', '—'),
      colour(signed(b.pnl), b.pnl), colour(pct(b.pnl_pct), b.pnl_pct),
      held(b.age_seconds), link(dexUrl(b.token)),
    ])) : el('div', 'empty', 'holding nothing'));

  body.append(el('div', 'mtitle', `POSITIONS FULLY CLOSED — ${t.history.length}`));
  body.append(t.history.length ? table(
    [{ label: 'TOKEN' }, { label: 'CLOSED' }, { label: 'PAID IN', n: true },
     { label: 'GOT OUT', n: true }, { label: 'MADE', n: true }, { label: 'RETURN', n: true },
     { label: 'HELD FOR', n: true }, { label: 'FILLS', n: true }],
    t.history.map((r) => [
      sym(r.symbol || '?', r.token), stamp(r.closed_ts),
      usd(r.cost_sold), usd(r.proceeds_usd),
      colour(signed(r.pnl_usd), r.pnl_usd), colour(pct(r.pnl_pct), r.pnl_pct),
      held(r.hold_seconds), `${r.buys}B/${r.sells}S`,
    ])) : el('div', 'empty', 'nothing closed yet'));
}

/* ------------------------------------------------------------ socket */

function setStatus(kind, text) { S.link = kind; S.linkText = text; drawRo(); }

// The socket itself lives in the Rust core and reconnects on its own. This
// side keeps the same bookkeeping the web client does: if the link is not up
// within a few seconds, or keeps dying, the tape is polled instead -- the same
// fills, about two seconds later -- so a blocked upgrade never freezes the tape.
let pollTimer = null;
let socketOpenedOnce = false;
function startPolling() {
  if (pollTimer) return;
  setStatus('warn', 'polling (no socket)');
  const tick = async () => {
    try {
      const rows = await api('/tape', { limit: 60, since_id: S.lastId || 0, stocks: 'true' });
      if (rows.length) pushFills(rows.reverse());
    } catch (e) { /* next tick */ }
  };
  tick();
  pollTimer = setInterval(tick, 2500);
}
function stopPolling() {
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
}

let socketFailures = 0;
async function connect() {
  const openTimeout = setTimeout(() => { if (!socketOpenedOnce) startPolling(); }, 4000);
  // listeners first, then the socket: the server's `hello` frame arrives the
  // instant the upgrade completes and must find someone listening
  await Promise.all([
    TAURI.event.listen('ws', (ev) => {
      const m = JSON.parse(ev.payload);
      if (m.type === 'fills') pushFills(m.data);
      else if (m.type === 'hello') applyStatus(m.data);
    }),
    TAURI.event.listen('link', (ev) => {
      if (ev.payload === 'open') {
        clearTimeout(openTimeout); socketOpenedOnce = true; socketFailures = 0; stopPolling();
        if (!S.replaying) setStatus('live', 'streaming');
      } else {
        socketFailures++;
        // a socket that never opened, or keeps dying, should not leave the tape frozen while it retries
        if (!socketOpenedOnce || socketFailures >= 2) startPolling();
        setStatus('down', pollTimer ? 'polling (no socket)' : 'reconnecting');
      }
    }),
  ]);
  invoke('ws_start').catch(() => startPolling());
}

function applyStatus(s) {
  LAST.status = s; drawRo();
  $('foot').textContent =
    `${s.wallets} wallets · ${NF0.format(s.trades || 0)} fills indexed · history from ${s.first_ts ? stamp(s.first_ts) : '—'} · chain ${s.chain_id} · read-only, no keys, no trading, nothing here is advice`;
}

/* ----------------------------------------------------------- refresh */

async function refresh() {
  try {
    const [o, traders, status] = await Promise.all([
      api('/overview', P()), api('/traders', P()), api('/status'),
    ]);
    S.traders = traders;
    drawStats(o); drawBoard(); applyStatus(status);
  } catch (e) { /* keep the last good view */ }
  drawClosed().catch(() => {});
  drawTab();
}

// Phones and Safari composite hundreds of animated table cells badly; a
// lighter boot there is the difference between a site and a frozen tab.
const TOUCH = window.matchMedia && window.matchMedia('(hover: none)').matches;
const REPLAY = TOUCH ? 40 : 120;

// Typed login lines, one after another, into the readout area.
const BOOT = [
  '> connecting to robinhood chain (4663) ........ ok',
  '> loading 108 tracked fomo.family wallets ..... ok',
  '> subscribing to transfer logs ................ ok',
  '> replaying the tape',
];
function boot() {
  return new Promise((done) => {
    S.booting = true;
    const ro = $('ro');
    const lines = [];
    let i = 0;
    const next = () => {
      if (i < BOOT.length) {
        lines.push(BOOT[i++]);
        ro.innerHTML = esc(lines.join('\n')) + '<span class="cur">█</span>';
        setTimeout(next, 170 + Math.random() * 120);
      } else {
        setTimeout(() => { S.booting = false; drawRo(); done(); }, 220);
      }
    };
    next();
  });
}

// On load the older rows are drawn instantly, then the last REPLAY fills are
// played back in bursts -- a few landing together, then a beat -- the way
// blocks actually arrive, speeding up as it goes. Live fills that arrive
// mid-replay are held and flushed after; pushFills dedupes by id.
async function loadTape() {
  const rows = await api('/tape', { limit: 400, stocks: 'true' });
  rows.sort((a, b) => a.id - b.id);
  const play = rows.slice(-REPLAY);
  const base = rows.slice(0, rows.length - play.length);
  S.fills = base.slice().reverse();
  S.lastId = base.length ? base[base.length - 1].id : 0;
  drawTape();
  await boot();
  if (!play.length) return;
  S.replaying = true;
  S.pending = [];
  setStatus('warn', `replaying last ${play.length} fills`);
  const wrap = $('tape').closest('.tw');
  let i = 0;
  const body = $('tape');
  const emit = (f) => {
    S.lastId = f.id;
    S.fills.unshift(f);
    if (passes(f)) {
      body.prepend(tapeRow(f, true));
      // the replay must honour the same cap as the live path, or the tape
      // sits at up to 270 rows until the first live fill trims it
      while (body.children.length > MAX_ROWS) body.removeChild(body.lastChild);
      const sus = (f.flags || []).length > 0;
      if (!sus && f.usd != null && f.usd >= BIG_USD) shout(f);
      if (!sus && f.usd != null && f.usd >= 250) blip(f.usd, f.side);
    }
    if (S.follow) wrap.scrollTop = 0;
  };
  const burst = () => {
    // burst size: mostly 1-2, sometimes a pile of 4-6
    const r = Math.random();
    const k = r < 0.45 ? 1 : r < 0.75 ? 2 : r < 0.9 ? 3 : 4 + Math.floor(Math.random() * 3);
    const group = play.slice(i, i + k);
    i += group.length;
    group.forEach((f, j) => setTimeout(() => emit(f), j * 18));
    if (i < play.length) {
      const left = 1 - i / play.length;                       // 1 -> 0
      setTimeout(burst, 35 + 210 * Math.pow(left, 1.4));      // ~245ms -> ~35ms between bursts
    } else {
      setTimeout(() => {
        S.replaying = false;
        const held = S.pending; S.pending = [];
        if (held.length) pushFills(held);
        setStatus('live', 'streaming');
      }, 150);
    }
  };
  burst();
}

/* -------------------------------------------------------------- wire */

function setWindow(w) {
  S.window = w;
  document.querySelectorAll('.kk[data-key]').forEach((k) => {
    k.classList.toggle('on', { '1': '1h', '2': '24h', '3': '7d', '4': '30d', '5': 'all' }[k.dataset.key] === w);
  });
  refresh();
}
function toggleSound() {
  S.sound = !S.sound;
  $('k-sound').textContent = S.sound ? (S.unlocked ? 'on' : 'on (click)') : 'off';
  if (S.sound) { unlockAudio(); setTimeout(() => blip(100, 'buy'), 50); }
}
function toggleStocks() { S.stocks = !S.stocks; $('k-stocks').textContent = S.stocks ? 'on' : 'off'; drawTape(); refresh(); }
function toggleFollow() { S.follow = !S.follow; $('k-follow').textContent = S.follow ? 'on' : 'off'; }

function wire() {
  const act = { '1': () => setWindow('1h'), '2': () => setWindow('24h'), '3': () => setWindow('7d'),
    '4': () => setWindow('30d'), '5': () => setWindow('all'), s: toggleSound, t: toggleStocks,
    f: toggleFollow, '/': () => $('tape-filter').focus(), r: () => openRpc() };
  document.querySelectorAll('.kk[data-key]').forEach((k) => k.addEventListener('click', () => act[k.dataset.key]()));
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') { $('modal').hidden = true; document.activeElement.blur(); return; }
    if (e.target.tagName === 'INPUT' || e.target.tagName === 'SELECT' || e.metaKey || e.ctrlKey) return;
    if (act[e.key]) { e.preventDefault(); act[e.key](); }
  });
  // every link on the page points off-app; hand them to the default browser
  // instead of navigating the tape away or spawning a bare webview window
  document.addEventListener('click', (e) => {
    const a = e.target.closest ? e.target.closest('a[href]') : null;
    if (!a) return;
    const href = a.getAttribute('href');
    if (!/^https?:\/\//i.test(href)) return;
    e.preventDefault();
    invoke('open_url', { url: href }).catch(() => {});
  });
  document.querySelectorAll('#tabbar .tab').forEach((b) => b.addEventListener('click', () => {
    document.querySelectorAll('#tabbar .tab').forEach((x) => x.classList.remove('on'));
    b.classList.add('on');
    document.querySelectorAll('.pane').forEach((p) => { p.hidden = true; });
    S.tab = b.dataset.tab; $(`tab-${S.tab}`).hidden = false; drawTab();
  }));
  $('lb-active').addEventListener('change', (e) => { S.activeOnly = e.target.checked; drawBoard(); });
  $('lb-sort').addEventListener('change', (e) => { S.sort = e.target.value; drawBoard(); });
  $('tape-filter').addEventListener('input', (e) => { S.filter = e.target.value.trim(); drawTape(); });
  $('modal-close').addEventListener('click', () => { $('modal').hidden = true; });
  $('modal').addEventListener('click', (e) => { if (e.target.id === 'modal') $('modal').hidden = true; });
  const tickClock = () => { $('clock').textContent = `${clock(Date.now() / 1000)} NYC`; drawSession(); };
  tickClock(); setInterval(tickClock, 1000);
  ['pointerdown', 'keydown', 'touchstart'].forEach((ev) =>
    document.addEventListener(ev, unlockAudio, { passive: true }));
  document.querySelector('.kk[data-key="2"]')?.classList.add('on');
}

wire();
loadTape().catch(() => {});
refresh();
connect();
setInterval(refresh, 15000);
