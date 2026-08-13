import { parseHttpUrl, urlMatches, navigationSucceeded } from "./urls.js";

const HOST = "com.alfred.browser_bridge";
const destructiveControl = /\b(empty\s+(trash|bin|recycle)|permanently\s+delete|delete\s+permanently|uninstall|wipe\s+disk|drop\s+table|delete\s+account|remove\s+user)\b/i;
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

async function resolveTab(message) {
  if (message.tabId !== undefined && message.tabId !== null) {
    const tab = await chrome.tabs.get(message.tabId);
    if (!tab?.id) throw new Error("The pinned browser tab no longer exists.");
    return tab;
  }
  return activeTab();
}

function waitTabComplete(tabId, expectedUrl, startUrl, timeoutMs = 15000) {
  return new Promise((resolve, reject) => {
    const started = Date.now();
    let committedUrl = null;
    const finish = (tab, error) => {
      clearInterval(timer);
      chrome.tabs.onUpdated.removeListener(onUpdated);
      if (error) reject(error);
      else resolve(tab);
    };
    const onUpdated = (id, info) => {
      if (id !== tabId) return;
      if (info.url) committedUrl = info.url;
    };
    chrome.tabs.onUpdated.addListener(onUpdated);
    const timer = setInterval(async () => {
      if (Date.now() - started >= timeoutMs) {
        finish(null, new Error("Navigation timed out waiting for the new page to load."));
        return;
      }
      try {
        const tab = await chrome.tabs.get(tabId);
        if (tab.status !== "complete") return;
        if (navigationSucceeded(startUrl, tab.url, expectedUrl, committedUrl)) finish(tab);
      } catch {
        finish(null, new Error("The tab disappeared during navigation."));
      }
    }, 200);
  });
}

async function pingTab(tabId) {
  try {
    const reply = await chrome.tabs.sendMessage(tabId, { method: "ping" });
    return reply?.pong === true;
  } catch {
    return false;
  }
}

async function sendToTab(tab, message) {
  try {
    return await chrome.tabs.sendMessage(tab.id, message);
  } catch {
    if (await pingTab(tab.id)) return chrome.tabs.sendMessage(tab.id, message);
    await chrome.scripting.executeScript({ target: { tabId: tab.id }, files: ["content.js"] });
    if (!(await pingTab(tab.id))) {
      throw new Error("The Alfred content script did not load in this tab.");
    }
    return chrome.tabs.sendMessage(tab.id, message);
  }
}

async function handleNativeMessage(message) {
  try {
    const observeLike = message.effect === "observe"
      || ["observe", "read", "scroll", "find", "wait", "status", "listTabs", "captureVisible", "ping"].includes(message.method);
    if (!observeLike && destructiveControl.test(JSON.stringify({
      method: message.method,
      target: message.target_label || message.targetLabel || "",
      url: message.url || "",
    }))) {
      throw new Error("Destructive browser actions are blocked by Alfred.");
    }
    let result;
    if (message.method === "status") result = { connected: true, version: chrome.runtime.getManifest().version };
    else if (message.method === "listTabs") result = (await chrome.tabs.query({})).map(({ id, title, url, active, windowId }) => ({ id, title, url, active, windowId }));
    else if (message.method === "navigate") {
      if (!parseHttpUrl(message.url)) {
        throw new Error("browser.navigate requires an absolute HTTP(S) URL.");
      }
      const tab = await resolveTab(message);
      if (urlMatches(tab.url, message.url)) {
        result = { tabId: tab.id, url: tab.url, ready: true, alreadyOnUrl: true };
      } else {
        const pending = waitTabComplete(tab.id, message.url, tab.url);
        const updated = await chrome.tabs.update(tab.id, { url: message.url });
        const ready = await pending;
        result = { tabId: updated.id, url: ready?.url || updated.url, ready: true };
      }
    }
    else if (message.method === "captureVisible") {
      const tab = await resolveTab(message);
      result = { tabId: tab.id, dataUrl: await chrome.tabs.captureVisibleTab(tab.windowId, { format: "png" }) };
    }
    else {
      const tab = await resolveTab(message);
      const inner = await sendToTab(tab, message);
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
