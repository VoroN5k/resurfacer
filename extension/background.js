// Resurfacer background service worker (MV3)
// Connects to the native messaging host, forwards tab events, and executes
// commands received from the daemon (close tab, extract content, reopen URLs).

const HOST_NAME = 'com.resurfacer.host';

// MV3 service workers can be suspended by Chrome after ~30 s of inactivity.
// We use chrome.alarms to keep the worker alive and reconnect the port when needed.
const KEEPALIVE_ALARM = 'resurfacer-keepalive';

let port = null;

// ── Port management ──────────────────────────────────────────────────────────

function connect() {
  port = chrome.runtime.connectNative(HOST_NAME);
  port.onMessage.addListener(onDaemonMessage);
  port.onDisconnect.addListener(() => {
    const err = chrome.runtime.lastError;
    if (err) console.error('[resurfacer] Disconnected:', err.message);
    port = null;
  });

  // Send a snapshot of all currently open tabs so the daemon can initialise its tracker.
  chrome.tabs.query({}, (tabs) => {
    for (const tab of tabs) {
      if (!tab.url || tab.url.startsWith('chrome://') || tab.url.startsWith('edge://')) continue;
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

  // Also report the currently focused tab.
  chrome.tabs.query({ active: true, currentWindow: true }, ([active]) => {
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

// ── Keepalive alarm ──────────────────────────────────────────────────────────

chrome.alarms.create(KEEPALIVE_ALARM, { periodInMinutes: 0.4 }); // ~24 s
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === KEEPALIVE_ALARM) ensureConnected();
});

// ── Tab event listeners ──────────────────────────────────────────────────────

chrome.tabs.onCreated.addListener((tab) => {
  if (!tab.url || tab.url.startsWith('chrome://') || tab.url.startsWith('edge://')) return;
  send({
    type: 'tab_created',
    tab_id: tab.id,
    url: tab.url || '',
    title: tab.title ?? null,
    opener_tab_id: tab.openerTabId ?? null,
    created_at: Date.now(),
  });
});

chrome.tabs.onActivated.addListener(({ tabId }) => {
  send({ type: 'tab_activated', tab_id: tabId });
});

chrome.tabs.onRemoved.addListener((tabId) => {
  send({ type: 'tab_removed', tab_id: tabId });
});

chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  if (changeInfo.status !== 'complete' && !changeInfo.url && !changeInfo.title) return;
  if (!tab.url || tab.url.startsWith('chrome://') || tab.url.startsWith('edge://')) return;
  send({
    type: 'tab_updated',
    tab_id: tabId,
    url: tab.url ?? null,
    title: tab.title ?? null,
    status: changeInfo.status ?? null,
  });
});

// ── Daemon command handler ───────────────────────────────────────────────────

function onDaemonMessage(msg) {
  switch (msg.type) {
    case 'request_content':
      extractContent(msg.tab_id);
      break;

    case 'close_tab':
      chrome.tabs.remove(msg.tab_id, () => {
        if (chrome.runtime.lastError) {
          console.warn('[resurfacer] Could not close tab', msg.tab_id, chrome.runtime.lastError.message);
        }
      });
      break;

    case 'reopen_urls':
      for (const url of msg.urls) {
        chrome.tabs.create({ url });
      }
      break;

    default:
      console.warn('[resurfacer] Unknown command type:', msg.type);
  }
}

// ── Content extraction ───────────────────────────────────────────────────────

async function extractContent(tabId) {
  try {
    const [result] = await chrome.scripting.executeScript({
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
    // Send empty content so the daemon can still archive and close the tab.
    send({ type: 'tab_content', tab_id: tabId, text: '', title: null });
  }
}

// Injected into the target page's context; must be a self-contained function.
function extractReadableText() {
  // Strip noise nodes in place (script, style, nav, footer, aside, ads).
  const noisy = ['script', 'style', 'nav', 'footer', 'aside', 'header',
                  'noscript', 'iframe', 'svg', 'form'];

  // Work on a clone so we don't mutate the live DOM.
  const clone = document.body?.cloneNode(true);
  if (!clone) return { text: '', title: document.title };

  for (const tag of noisy) {
    clone.querySelectorAll(tag).forEach((el) => el.remove());
  }

  // Collect all non-empty text from paragraph-like elements first.
  const blocks = clone.querySelectorAll('p, article, section, main, .content, .post, .article');
  let text = '';

  if (blocks.length > 0) {
    // Find the block with the most text.
    let best = '';
    for (const block of blocks) {
      const t = (block.textContent ?? '').replace(/\s+/g, ' ').trim();
      if (t.length > best.length) best = t;
    }
    text = best;
  }

  // Fallback: use the whole body's text.
  if (text.length < 200) {
    text = (clone.textContent ?? '').replace(/\s+/g, ' ').trim();
  }

  // Truncate to ~2000 chars (the daemon will further limit to excerpt_word_limit).
  return { text: text.slice(0, 2000), title: document.title };
}

// ── Initialise on service worker start ──────────────────────────────────────
connect();
