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

async function handleNativeMessage(message) {
  try {
    if (message.effect !== "observe" && destructive.test(JSON.stringify(message)))
      throw new Error("Destructive browser actions are blocked by Alfred.");
    let result;
    if (message.method === "status") result = { connected: true, version: chrome.runtime.getManifest().version };
    else if (message.method === "listTabs") result = (await chrome.tabs.query({})).map(({ id, title, url, active, windowId }) => ({ id, title, url, active, windowId }));
    else if (message.method === "navigate") { const tab = await activeTab(); result = await chrome.tabs.update(tab.id, { url: message.url }); }
    else if (message.method === "captureVisible") { const tab = await activeTab(); result = { tabId: tab.id, dataUrl: await chrome.tabs.captureVisibleTab(tab.windowId, { format: "png" }) }; }
    else { const tab = await activeTab(); result = await chrome.tabs.sendMessage(tab.id, message); }
    nativePort?.postMessage({ id: message.id, ok: true, result });
  } catch (error) {
    nativePort?.postMessage({ id: message.id, ok: false, error: String(error?.message ?? error) });
  }
}

chrome.runtime.onInstalled.addListener(connect);
chrome.runtime.onStartup.addListener(connect);
connect();
