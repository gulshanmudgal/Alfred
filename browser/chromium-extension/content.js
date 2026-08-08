(() => {
  const refs = new Map();
  const destructive = /\b(delete|remove|erase|trash|purge|wipe|shred|empty\s+(trash|bin|recycle))\b/i;
  const visible = element => { const rect = element.getBoundingClientRect(); const style = getComputedStyle(element); return rect.width > 0 && rect.height > 0 && style.visibility !== "hidden" && style.display !== "none"; };
  const label = element => element.getAttribute("aria-label") || element.innerText?.trim() || element.getAttribute("placeholder") || element.getAttribute("title") || element.getAttribute("name") || "";
  const observe = () => {
    refs.clear();
    const selector = "a[href],button,input,textarea,select,[role=button],[role=link],[contenteditable=true]";
    const elements = [...document.querySelectorAll(selector)].filter(visible).slice(0, 500);
    return { url: location.href, title: document.title, elements: elements.map((element, index) => {
      const ref = `e${index + 1}`; refs.set(ref, element); const rect = element.getBoundingClientRect();
      return { ref, tag: element.tagName.toLowerCase(), role: element.getAttribute("role"), label: label(element).slice(0, 300), type: element.getAttribute("type"), disabled: element.matches(":disabled"), bounds: { x: rect.x, y: rect.y, width: rect.width, height: rect.height } };
    }) };
  };
  const elementFor = ref => refs.get(ref) || (() => { throw new Error("Element reference expired; observe the page again."); })();
  chrome.runtime.onMessage.addListener((message, _sender, respond) => {
    Promise.resolve().then(() => {
      if (message.method === "observe") return observe();
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
      throw new Error(`Unsupported browser method: ${message.method}`);
    }).then(respond).catch(error => respond({ error: String(error.message ?? error) }));
    return true;
  });
})();
