(() => {
  const refs = new Map();
  const destructive = /\b(delete|remove|erase|trash|purge|wipe|shred|empty\s+(trash|bin|recycle))\b/i;
  const visible = element => { const rect = element.getBoundingClientRect(); const style = getComputedStyle(element); return rect.width > 0 && rect.height > 0 && style.visibility !== "hidden" && style.display !== "none"; };
  const label = element => element.getAttribute("aria-label") || element.innerText?.trim() || element.getAttribute("placeholder") || element.getAttribute("title") || element.getAttribute("name") || "";
  // Shadow-piercing query: SPA shells hide controls inside shadow roots that
  // querySelectorAll cannot reach.
  const deepQueryAll = (selector, root = document) => {
    const found = [...root.querySelectorAll(selector)];
    for (const element of root.querySelectorAll("*")) {
      if (element.shadowRoot) found.push(...deepQueryAll(selector, element.shadowRoot));
    }
    return found;
  };
  const READ_CHUNK = 6000;
  const READ_POOL = 24000;
  // Extracts readable page content (what observe cannot see: articles, tables,
  // error lists) as plain text, chunked so the planner can page through long
  // documents instead of flooding one prompt.
  const readPage = (message) => {
    const roots = ["article", "main", "[role=main]", "#content", "#main"]
      .map((selector) => { try { return document.querySelector(selector); } catch { return null; } })
      .filter((element) => element && (element.innerText || "").trim().length > 200);
    const root = roots.sort((a, b) => (b.innerText || "").length - (a.innerText || "").length)[0] || document.body;
    let text = (root.innerText || "").trim();
    const shadowChunks = [];
    const collectShadow = (scope) => {
      for (const element of scope.querySelectorAll("*")) {
        if (!element.shadowRoot) continue;
        for (const child of element.shadowRoot.children) {
          const chunk = (child.innerText || "").trim();
          if (chunk && !text.includes(chunk.slice(0, 80))) shadowChunks.push(chunk);
        }
        collectShadow(element.shadowRoot);
      }
    };
    collectShadow(document);
    if (shadowChunks.length) text += "\n" + shadowChunks.join("\n");
    text = text.replace(/\n{3,}/g, "\n\n");
    const totalChars = text.length;
    const pooled = text.slice(0, READ_POOL);
    const offset = Math.max(0, Number(message.offset) || 0);
    const chunkText = pooled.slice(offset, offset + READ_CHUNK);
    return { url: location.href, title: document.title, text: chunkText, offset, nextOffset: offset + chunkText.length, hasMore: offset + chunkText.length < pooled.length, truncated: totalChars > pooled.length, totalChars };
  };
  const scrollPage = (message) => {
    if (message.text) {
      const needle = String(message.text).toLowerCase();
      const match = deepQueryAll("h1,h2,h3,h4,td,th,p,span,li,a,div").find((element) => visible(element) && (element.innerText || "").toLowerCase().includes(needle));
      if (!match) throw new Error(`No visible text matches "${String(message.text).slice(0, 80)}".`);
      match.scrollIntoView({ block: "center" });
      return { scrolled: true, matched: String(message.text).slice(0, 120) };
    }
    const amount = Math.max(200, Math.floor(window.innerHeight * 0.85));
    const before = window.scrollY;
    window.scrollBy(0, message.direction === "up" ? -amount : amount);
    const max = Math.max(0, document.documentElement.scrollHeight - window.innerHeight);
    return { scrolled: window.scrollY !== before, scrollY: Math.round(window.scrollY), maxScroll: Math.round(max), atStart: window.scrollY <= 2, atEnd: window.scrollY >= max - 2 };
  };
  const observe = () => {
    refs.clear();
    const selector = "a[href],button,input,textarea,select,[role=button],[role=link],[contenteditable=true]";
    const elements = deepQueryAll(selector).filter(visible).slice(0, 500);
    return { url: location.href, title: document.title, elements: elements.map((element, index) => {
      const ref = `e${index + 1}`; refs.set(ref, element); const rect = element.getBoundingClientRect();
      return { ref, tag: element.tagName.toLowerCase(), role: element.getAttribute("role"), label: label(element).slice(0, 300), type: element.getAttribute("type"), disabled: element.matches(":disabled"), bounds: { x: rect.x, y: rect.y, width: rect.width, height: rect.height } };
    }) };
  };
  const elementFor = ref => refs.get(ref) || (() => { throw new Error("Element reference expired; observe the page again."); })();
  chrome.runtime.onMessage.addListener((message, _sender, respond) => {
    Promise.resolve().then(() => {
      if (message.method === "observe") return observe();
      // Read-only methods sit before the destructive-language gate so a search
      // string like "deleted errors" can never trip it; neither can change page
      // state.
      if (message.method === "read") return readPage(message);
      if (message.method === "scroll") return scrollPage(message);
      if (destructive.test(JSON.stringify(message))) throw new Error("Destructive browser actions are blocked by Alfred.");
      const element = elementFor(message.ref);
      if (message.method === "click") { element.scrollIntoView({ block: "center" }); element.click(); return { clicked: true }; }
      if (message.method === "type") {
        if (element.getAttribute("type") === "password") throw new Error("Alfred never types into password fields; use the browser password manager.");
        element.focus(); if ("value" in element) element.value = message.text; else element.textContent = message.text;
        element.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: message.text }));
        element.dispatchEvent(new Event("change", { bubbles: true }));
        return { typed: true, characters: message.text.length };
      }
      if (message.method === "getText") {
        const text = (("value" in element ? element.value : element.innerText?.trim()) || label(element)).slice(0, 2000);
        return { text };
      }
      throw new Error(`Unsupported browser method: ${message.method}`);
    }).then(respond).catch(error => respond({ error: String(error.message ?? error) }));
    return true;
  });
})();
