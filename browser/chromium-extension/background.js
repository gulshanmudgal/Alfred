const HOST = "com.alfred.browser_bridge";
const destructive = /\b(delete|remove|erase|trash|purge|wipe|shred|empty\s+(trash|bin|recycle))\b/i;
let nativePort;

function connect() {
  if (nativePort) return;
  nativePort = chrome.runtime.connectNative(HOST);
  nativePort.onMessage.addListener(handleNativeMessage);
  nativePort.onDisconnect.addListener(() => { nativePort = undefined; });
}

async function activeTab() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (!tab?.id) throw new Error("No active browser tab is available.");
  return tab;
}

// A run pins the tab it is working in. Commands that carry a tabId act on that tab
// even when the user or another window has changed which tab is active.
async function resolveTab(message) {
  if (message.tabId !== undefined && message.tabId !== null) {
    const tab = await chrome.tabs.get(message.tabId);
    if (!tab?.id) throw new Error("The pinned browser tab no longer exists.");
    return tab;
  }
  return activeTab();
}

async function handleNativeMessage(message) {
  try {
    if (message.effect !== "observe" && destructive.test(JSON.stringify(message)))
      throw new Error("Destructive browser actions are blocked by Alfred.");
    let result;
    if (message.method === "status") result = { connected: true, version: chrome.runtime.getManifest().version };
    else if (message.method === "listTabs") result = (await chrome.tabs.query({})).map(({ id, title, url, active, windowId }) => ({ id, title, url, active, windowId }));
    else if (message.method === "navigate") { const tab = await resolveTab(message); const updated = await chrome.tabs.update(tab.id, { url: message.url }); result = { tabId: updated.id, url: updated.url }; }
    else if (message.method === "captureVisible") { const tab = await resolveTab(message); result = { tabId: tab.id, dataUrl: await chrome.tabs.captureVisibleTab(tab.windowId, { format: "png" }) }; }
    else {
      const tab = await resolveTab(message);
      const inner = await chrome.tabs.sendMessage(tab.id, message);
      // The content script reports failures in-band; surface them as real failures.
      if (inner?.error) throw new Error(inner.error);
      result = { ...inner, tabId: tab.id };
    }
    nativePort?.postMessage({ id: message.id, ok: true, result });
  } catch (error) {
    nativePort?.postMessage({ id: message.id, ok: false, error: String(error?.message ?? error) });
  }
}

chrome.runtime.onInstalled.addListener(connect);
chrome.runtime.onStartup.addListener(connect);
connect();
