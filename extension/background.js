// Resurfacer background service worker (MV3) - Chrome, Edge, Firefox
//
// Firefox exposes 'browser' (Promise-based); Chrome/Edge expose 'chrome'
// (callback-based). Firefox also aliases 'chrome', but we prefer 'browser'
// so we get native Promises in Firefox without a polyfill
const api = (typeof browser !== 'undefined' && typeof browser.runtime !== 'undefined')
  ? browser
  : chrome;

const HOST_NAME = 'com.resurfacer.host';

// MV3 service workers can be suspended after ~30 s of inactivity
// We use alarms to keep the worker alive and reconnect the port when needed
const KEEPALIVE_ALARM = 'resurfacer-keepalive';

let port = null;

// Internal URL prefixes to skip - browser UI pages that can't be scripted
const SKIP_PREFIXES = ['chrome://', 'edge://', 'about:', 'moz-extension://', 'chrome-extension://'];

function isScriptable(url) {
  if (!url) return false;
  return !SKIP_PREFIXES.some((p) => url.startsWith(p));
}

// Port management

function connect() {
  port = api.runtime.connectNative(HOST_NAME);
  port.onMessage.addListener(onDaemonMessage);
  port.onDisconnect.addListener(() => {
    const err = api.runtime.lastError;
    if (err) console.error('[resurfacer] Disconnected:', err.message);
    port = null;
  });

  // Send a snapshot of all currently open tabs so the daemon can initialise its tracker
  api.tabs.query({}, (tabs) => {
    for (const tab of tabs) {
      if (!isScriptable(tab.url)) continue;
      send({
        type: 'tab_created',
        tab_id: tab.id,
        url: tab.url,
        title: tab.title ?? null,
        opener_tab_id: null,   // unknown for pre-existing tabs
        created_at: Date.now(),
      });
    }
  });

  // Also report the currently focused tab
  api.tabs.query({ active: true, currentWindow: true }, ([active]) => {
    if (active) send({ type: 'tab_activated', tab_id: active.id });
  });

  console.log('[resurfacer] Connected to native host');
}

function ensureConnected() {
  if (!port) connect();
}

function send(msg) {
  ensureConnected();
  try {
    port.postMessage(msg);
  } catch (e) {
    console.error('[resurfacer] postMessage failed:', e);
    port = null;
  }
}

// Keepalive alarm

api.alarms.create(KEEPALIVE_ALARM, { periodInMinutes: 0.4 }); // ~24 s
api.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === KEEPALIVE_ALARM) ensureConnected();
});

// Tab event listeners

api.tabs.onCreated.addListener((tab) => {
  const url = tab.url || '';
  // Firefox sets url='about:blank' at creation time; the real URL arrives via
  // tab_updated once navigation starts. Block only extension-internal pages
  // that truly never change - not transient about:blank
  if (url.startsWith('moz-extension://') || url.startsWith('chrome-extension://')) return;
  console.log('[resurfacer] tab_created', tab.id, url || '(blank)', 'opener:', tab.openerTabId ?? null);
  send({
    type: 'tab_created',
    tab_id: tab.id,
    url: url,
    title: tab.title ?? null,
    opener_tab_id: tab.openerTabId ?? null,
    created_at: Date.now(),
  });
});

api.tabs.onActivated.addListener(({ tabId }) => {
  send({ type: 'tab_activated', tab_id: tabId });
});

api.tabs.onRemoved.addListener((tabId) => {
  send({ type: 'tab_removed', tab_id: tabId });
});

api.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  if (changeInfo.status !== 'complete' && !changeInfo.url && !changeInfo.title) return;
  if (!isScriptable(tab.url)) return;
  console.log('[resurfacer] tab_updated', tabId, changeInfo.status, tab.url);
  send({
    type: 'tab_updated',
    tab_id: tabId,
    url: tab.url ?? null,
    title: tab.title ?? null,
    status: changeInfo.status ?? null,
  });
});

// Daemon command handler

function onDaemonMessage(msg) {
  switch (msg.type) {
    case 'request_content':
      extractContent(msg.tab_id);
      break;

    case 'close_tab':
      api.tabs.remove(msg.tab_id, () => {
        if (api.runtime.lastError) {
          console.warn('[resurfacer] Could not close tab', msg.tab_id, api.runtime.lastError.message);
        }
      });
      break;

    case 'reopen_urls':
      for (const url of msg.urls) {
        api.tabs.create({ url });
      }
      break;

    default:
      console.warn('[resurfacer] Unknown command type:', msg.type);
  }
}

// Content extraction

async function extractContent(tabId) {
  try {
    const [result] = await api.scripting.executeScript({
      target: { tabId },
      func: extractReadableText,
    });

    send({
      type: 'tab_content',
      tab_id: tabId,
      text: result?.result?.text ?? '',
      title: result?.result?.title ?? null,
    });
  } catch (e) {
    console.warn('[resurfacer] Content extraction failed for tab', tabId, e.message);
    // Send empty content so the daemon can still archive and close the tab
    send({ type: 'tab_content', tab_id: tabId, text: '', title: null });
  }
}

// Injected into the target page's context; must be a self-contained function
function extractReadableText() {
  // Strip noise nodes in place (script, style, nav, footer, aside, ads)
  const noisy = ['script', 'style', 'nav', 'footer', 'aside', 'header',
                  'noscript', 'iframe', 'svg', 'form'];

  // Work on a clone so we don't mutate the live DOM
  const clone = document.body?.cloneNode(true);
  if (!clone) return { text: '', title: document.title };

  for (const tag of noisy) {
    clone.querySelectorAll(tag).forEach((el) => el.remove());
  }

  // Collect all non-empty text from paragraph-like elements first
  const blocks = clone.querySelectorAll('p, article, section, main, .content, .post, .article');
  let text = '';

  if (blocks.length > 0) {
    // Find the block with the most text
    let best = '';
    for (const block of blocks) {
      const t = (block.textContent ?? '').replace(/\s+/g, ' ').trim();
      if (t.length > best.length) best = t;
    }
    text = best;
  }

  // Fallback: use the whole body's text
  if (text.length < 200) {
    text = (clone.textContent ?? '').replace(/\s+/g, ' ').trim();
  }

  // Truncate to ~2000 chars (the daemon will further limit to excerpt_word_limit)
  return { text: text.slice(0, 2000), title: document.title };
}

// Initialise on service worker start
connect();
