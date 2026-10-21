// Save States companion.
//
// Reports this profile's windows and tabs to the Save States desktop app through its
// native-messaging bridge, and on restore applies what Chrome's command line can't:
// pinned tabs, tab groups and the active tab.
//
// MV3 service workers sleep after ~30s idle, which also drops the native port. That
// is harmless here: every tab/window event wakes the worker and pushes a full
// snapshot, so no change is missed, and the bridge's periodic ping keeps the worker
// awake while the port is open. An alarm reconnects if the port ever drops.

const HOST = "com.savestates.bridge";
const VERSION = chrome.runtime.getManifest().version;
const PUSH_DEBOUNCE_MS = 750;

let port = null;
let instanceId = null;
let pushTimer = null;

async function getInstanceId() {
  if (instanceId) return instanceId;
  const stored = await chrome.storage.local.get("instanceId");
  instanceId = stored.instanceId || crypto.randomUUID();
  if (!stored.instanceId) await chrome.storage.local.set({ instanceId });
  return instanceId;
}

async function snapshot() {
  const windows = await chrome.windows.getAll({ populate: true, windowTypes: ["normal", "popup", "app"] });
  let groups = [];
  try {
    groups = await chrome.tabGroups.query({});
  } catch (_) {
    // tabGroups unavailable (very old Chrome or policy); tabs still work.
  }
  return {
    type: "snapshot",
    instance: await getInstanceId(),
    extVersion: VERSION,
    ts: Date.now(),
    windows: windows.map((w) => ({
      id: w.id,
      focused: !!w.focused,
      incognito: !!w.incognito,
      state: w.state,
      type: w.type,
      left: w.left ?? 0,
      top: w.top ?? 0,
      width: w.width ?? 0,
      height: w.height ?? 0,
      tabs: (w.tabs || []).map((t) => ({
        id: t.id,
        index: t.index,
        url: t.url || t.pendingUrl || "",
        title: t.title || "",
        pinned: !!t.pinned,
        active: !!t.active,
        groupId: t.groupId ?? -1,
      })),
    })),
    groups: groups.map((g) => ({
      id: g.id,
      windowId: g.windowId,
      title: g.title || "",
      color: g.color,
      collapsed: !!g.collapsed,
    })),
  };
}

function send(msg) {
  if (!port) return false;
  try {
    port.postMessage(msg);
    return true;
  } catch (_) {
    return false;
  }
}

function ensurePort() {
  if (port) return true;
  try {
    port = chrome.runtime.connectNative(HOST);
  } catch (_) {
    port = null;
    return false;
  }
  port.onMessage.addListener(onHostMessage);
  port.onDisconnect.addListener(() => {
    // Host not registered yet, or it exited. Reading lastError marks it handled;
    // the alarm below retries.
    void chrome.runtime.lastError;
    port = null;
  });
  getInstanceId().then((instance) => {
    send({ type: "hello", instance, extVersion: VERSION });
    schedulePush();
  });
  return true;
}

function schedulePush() {
  clearTimeout(pushTimer);
  pushTimer = setTimeout(async () => {
    if (ensurePort()) send(await snapshot());
  }, PUSH_DEBOUNCE_MS);
}

async function onHostMessage(msg) {
  if (!msg || typeof msg !== "object") return;
  if (msg.type === "ping") {
    send({ type: "pong" });
    return;
  }
  if (msg.type !== "request") return;
  if (msg.op === "snapshot") {
    send({ ...(await snapshot()), req: msg.req });
  } else if (msg.op === "apply") {
    try {
      const applied = await applySpec(msg.spec || {});
      send({ type: "result", req: msg.req, ok: applied, error: applied ? null : "no matching window" });
    } catch (e) {
      send({ type: "result", req: msg.req, ok: false, error: String(e) });
    }
  }
}

const norm = (u) => (u || "").trim().replace(/\/+$/, "");

// The app has just opened a window with these URLs; find it and apply the rest.
async function applySpec(spec) {
  const urls = (spec.urls || []).map(norm);
  for (let attempt = 0; attempt < 10; attempt++) {
    const windows = await chrome.windows.getAll({ populate: true, windowTypes: ["normal"] });
    const target = bestWindow(windows, urls);
    if (target) {
      await applyTo(target, spec);
      return true;
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  return false;
}

function bestWindow(windows, urls) {
  let best = null;
  let bestScore = 0;
  for (const w of windows) {
    const tabs = [...(w.tabs || [])].sort((a, b) => a.index - b.index);
    if (tabs.length !== urls.length) continue;
    let score = 0;
    tabs.forEach((t, i) => {
      if (norm(t.url || t.pendingUrl) === urls[i]) score++;
    });
    const enough = score >= Math.ceil(urls.length * 0.6);
    // On a tie the newest window wins: it is the one the app just created.
    if (enough && (score > bestScore || (score === bestScore && best && w.id > best.id))) {
      best = w;
      bestScore = score;
    }
  }
  return best;
}

async function applyTo(win, spec) {
  const tabs = [...win.tabs].sort((a, b) => a.index - b.index);
  const tabId = (i) => (Number.isInteger(i) && tabs[i] ? tabs[i].id : null);
  // Pinned tabs are always the leading tabs, so pinning in index order keeps positions.
  for (const i of [...(spec.pinned || [])].sort((a, b) => a - b)) {
    const id = tabId(i);
    if (id != null) await chrome.tabs.update(id, { pinned: true });
  }
  for (const g of spec.groups || []) {
    const tabIds = (g.indices || []).map(tabId).filter((id) => id != null);
    if (!tabIds.length) continue;
    const groupId = await chrome.tabs.group({ tabIds, createProperties: { windowId: win.id } });
    await chrome.tabGroups.update(groupId, { title: g.title || "", color: g.color || "grey", collapsed: !!g.collapsed });
  }
  const active = tabId(spec.active);
  if (active != null) await chrome.tabs.update(active, { active: true });
}

// Every change pushes a fresh snapshot. Listeners must be registered synchronously at
// startup so that events can wake the worker.
for (const event of [
  chrome.tabs.onCreated,
  chrome.tabs.onRemoved,
  chrome.tabs.onMoved,
  chrome.tabs.onAttached,
  chrome.tabs.onDetached,
  chrome.tabs.onActivated,
  chrome.tabs.onReplaced,
  chrome.windows.onCreated,
  chrome.windows.onRemoved,
  chrome.windows.onBoundsChanged,
  chrome.tabGroups.onCreated,
  chrome.tabGroups.onUpdated,
  chrome.tabGroups.onRemoved,
  chrome.tabGroups.onMoved,
]) {
  event.addListener(schedulePush);
}
// onUpdated also fires for loading progress and favicons; only these changes matter.
chrome.tabs.onUpdated.addListener((_tabId, change) => {
  if ("url" in change || "title" in change || "pinned" in change || "groupId" in change) schedulePush();
});

chrome.alarms.create("savestates-reconnect", { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === "savestates-reconnect" && !port) {
    ensurePort();
  }
});
chrome.runtime.onStartup.addListener(ensurePort);
chrome.runtime.onInstalled.addListener(ensurePort);

ensurePort();
