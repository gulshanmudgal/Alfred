(() => {
  if (globalThis.__alfredBridgeInstalled) return;
  globalThis.__alfredBridgeInstalled = true;

  const refs = new Map();
  const REVERSIBLE_HINTS = new Set(["filter", "draft", "selection", "highlight", "formatting"]);
  const DESTRUCTIVE_NOUNS = new Set([
    "account", "user", "file", "email", "message", "item", "post", "mail",
    "member", "record", "folder", "data", "project", "task", "workspace", "permanently",
  ]);
  const DESTRUCTION_VERBS = new Set([
    "delete", "remove", "erase", "destroy", "purge", "uninstall", "trash", "overwrite", "wipe",
  ]);
  const policyTokens = (value) =>
    String(value || "")
      .toLowerCase()
      .split(/[^a-z0-9]+/)
      .filter(Boolean);
  const verbIsReversible = (tokens, index) => {
    const verb = tokens[index];
    if (verb !== "delete" && verb !== "remove" && verb !== "erase") return false;
    if (tokens.some((token) => DESTRUCTIVE_NOUNS.has(token))) return false;
    const window = tokens.slice(index, index + 4);
    if (!window.some((token) => REVERSIBLE_HINTS.has(token))) return false;
    if (window.includes("draft") &&
      !window.some((token) => token === "text" || token === "selection" || token === "highlight" || token === "formatting")) {
      return false;
    }
    return true;
  };
  const isDestructionLabel = (value) => {
    const text = String(value || "").trim();
    if (!text) return false;
    if (/\b(empty\s+(trash|bin|recycle)|permanently\s+delete|delete\s+permanently|uninstall|wipe\s+disk|drop\s+table)\b/i.test(text)) {
      return true;
    }
    const tokens = policyTokens(text);
    return tokens.some((token, index) => DESTRUCTION_VERBS.has(token) && !verbIsReversible(tokens, index));
  };
  const isConfirmationLabel = (value) =>
    /^(confirm|yes|ok|okay|continue|proceed|accept|apply|i understand)$/i.test(String(value || "").trim());
  const isDismissiveLabel = (value) =>
    /^(cancel|no|close|dismiss|back|never mind|not now|keep)$/i.test(String(value || "").trim());
  const ownerView = (element) => element?.ownerDocument?.defaultView || window;
  const ancestorContext = (element) => {
    const parts = [];
    const dialog = element.closest?.("dialog, [role='dialog'], [role='alertdialog'], [aria-modal='true']");
    if (dialog) {
      parts.push(dialog.getAttribute("aria-label") || "", (dialog.innerText || "").slice(0, 400));
    }
    let node = element;
    for (let i = 0; i < 6 && node && node !== document.body && node !== document.documentElement; i++) {
      parts.push(node.getAttribute?.("aria-label") || "");
      parts.push(node.getAttribute?.("title") || "");
      const labelledBy = node.getAttribute?.("aria-labelledby");
      if (labelledBy) {
        const root = node.getRootNode?.() || node.ownerDocument || document;
        const lookup = (id) =>
          root.getElementById
            ? root.getElementById(id)
            : (node.ownerDocument || document).getElementById(id);
        for (const id of String(labelledBy).split(/\s+/).filter(Boolean)) {
          const resolved = lookup(id);
          parts.push(resolved?.innerText || resolved?.textContent || "");
        }
      }
      node = node.parentElement;
    }
    return parts.filter(Boolean).join(" ");
  };
  const visible = (element) => {
    const view = ownerView(element);
    if (!view) return false;
    const rect = element.getBoundingClientRect();
    const style = view.getComputedStyle(element);
    return rect.width > 0 && rect.height > 0 && style.visibility !== "hidden" && style.display !== "none";
  };
  const label = (element) =>
    element.getAttribute("aria-label") ||
    element.innerText?.trim() ||
    element.getAttribute("placeholder") ||
    element.getAttribute("title") ||
    element.getAttribute("name") ||
    "";

  // Shadow-piercing + same-origin iframe query. Cross-origin frames stay closed.
  const deepQueryAll = (selector, root = document, seen = new Set()) => {
    if (!root || seen.has(root)) return [];
    seen.add(root);
    const found = [...root.querySelectorAll(selector)];
    for (const element of root.querySelectorAll("*")) {
      if (element.shadowRoot) found.push(...deepQueryAll(selector, element.shadowRoot, seen));
    }
    for (const frame of root.querySelectorAll("iframe")) {
      try {
        const doc = frame.contentDocument;
        if (doc) found.push(...deepQueryAll(selector, doc, seen));
      } catch {
        /* Cross-origin iframe. */
      }
    }
    return found;
  };

  const READ_CHUNK = 6000;
  const READ_POOL = 32000;

  const frameDocuments = () => {
    const docs = [document];
    const seen = new Set([document]);
    for (const frame of deepQueryAll("iframe")) {
      try {
        const doc = frame.contentDocument;
        if (doc && !seen.has(doc)) {
          seen.add(doc);
          docs.push(doc);
        }
      } catch {
        /* Cross-origin iframe. */
      }
    }
    return docs;
  };

  const loginSignals = () => {
    const body = frameDocuments()
      .map((doc) => doc.body?.innerText || "")
      .join("\n")
      .toLowerCase();
    const title = (document.title || "").toLowerCase();
    const passwordField = deepQueryAll('input[type="password"]').some(visible);
    const captcha =
      !!document.querySelector("iframe[src*='recaptcha'], iframe[src*='hcaptcha'], .g-recaptcha, #cf-challenge-running") ||
      /\bcaptcha\b|\bverify you are human\b|\bcloudflare\b/.test(body);
    const loginWall =
      passwordField ||
      /\bsign in\b|\blog in\b|\blogin\b|\bsso\b|\bauthenticate\b/.test(title) ||
      (passwordField && /\bemail\b|\busername\b/.test(body));
    return { loginWall, captcha, passwordField };
  };

  const COMPOSER_SELECTOR = [
    "textarea",
    "[contenteditable=true]",
    "[contenteditable='']",
    "[role=textbox]",
    "input:not([type])",
    "input[type='']",
    "input[type=text]",
    "input[type=search]",
    "input[type=email]",
    "input[type=url]",
    "input[type=tel]",
    "input[type=password]",
    "input[type=number]",
  ].join(",");

  const structuredText = () => {
    const parts = [];
    const push = (chunk) => {
      const text = (chunk || "").trim();
      if (text) parts.push(text);
    };
    const composerElements = deepQueryAll(COMPOSER_SELECTOR).filter((element) => {
      if (element.matches?.(":disabled") || element.disabled) return false;
      if (element.readOnly || element.getAttribute("aria-readonly") === "true") return false;
      return true;
    });
    const containsComposer = (node) =>
      composerElements.some((composer) => node === composer || node.contains(composer) || composer.contains(node));
    const visibleText = (node, limit) => {
      if (containsComposer(node)) return "";
      return (node.innerText || "").trim().replace(/\s+/g, " ").slice(0, limit);
    };

    for (const heading of deepQueryAll("h1,h2,h3,h4,[role='heading']").filter(visible).slice(0, 40)) {
      const headingText = visibleText(heading, 200);
      if (headingText) push(`# ${headingText}`);
    }

    for (const table of deepQueryAll("table").filter(visible).slice(0, 12)) {
      const rows = [...table.querySelectorAll("tr")].slice(0, 40).map((row) =>
        [...row.querySelectorAll("th,td")]
          .map((cell) => visibleText(cell, 120))
          .filter(Boolean)
          .join(" | ")
      );
      const body = rows.filter(Boolean).join("\n");
      if (body) push(`TABLE:\n${body}`);
    }

    for (const list of deepQueryAll("ul,ol,[role='list']").filter(visible).slice(0, 20)) {
      const items = [...list.querySelectorAll(":scope > li, :scope > [role='listitem']")]
        .filter(visible)
        .slice(0, 40)
        .map((item) => {
          const text = visibleText(item, 200);
          return text ? `- ${text}` : "";
        })
        .filter((line) => line.length > 3);
      if (items.length) push(`LIST:\n${items.join("\n")}`);
    }

    for (const grid of deepQueryAll("[role='grid'], [role='table']").filter(visible).slice(0, 8)) {
      const cells = [...grid.querySelectorAll("[role='row']")]
        .slice(0, 40)
        .map((row) =>
          [...row.querySelectorAll("[role='gridcell'], [role='columnheader'], [role='cell']")]
            .map((cell) => visibleText(cell, 100))
            .filter(Boolean)
            .join(" | ")
        )
        .filter(Boolean);
      if (cells.length) push(`GRID:\n${cells.join("\n")}`);
    }

    const roots = [];
    for (const doc of frameDocuments()) {
      for (const selector of ["article", "main", "[role=main]", "#content", "#main"]) {
        try {
          const found = doc.querySelector(selector);
          if (found && (found.innerText || "").trim().length > 80) roots.push(found);
        } catch {
          /* Invalid selector in this document. */
        }
      }
    }
    const root =
      roots.sort((a, b) => (b.innerText || "").length - (a.innerText || "").length)[0]
      || document.body;
    const textWithoutComposers = (node) => {
      if (!node) return "";
      try {
        const clone = node.cloneNode(true);
        clone.querySelectorAll?.(COMPOSER_SELECTOR)?.forEach((element) => element.remove());
        return (clone.innerText || "").trim();
      } catch {
        return (node.innerText || "").trim();
      }
    };
    let prose = textWithoutComposers(root);

    const shadowChunks = [];
    const collectShadow = (scope) => {
      for (const element of scope.querySelectorAll("*")) {
        if (!element.shadowRoot) continue;
        for (const child of element.shadowRoot.children) {
          const chunk = textWithoutComposers(child);
          if (chunk && !prose.includes(chunk.slice(0, 80))) shadowChunks.push(chunk);
        }
        collectShadow(element.shadowRoot);
      }
    };
    for (const doc of frameDocuments()) collectShadow(doc);
    if (shadowChunks.length) prose += "\n" + shadowChunks.join("\n");

    const structured = parts.join("\n\n");

    return {
      structured: structured.replace(/\n{3,}/g, "\n\n"),
      prose: prose.replace(/\n{3,}/g, "\n\n").trim(),
    };
  };

  const readPage = (message) => {
    const signals = loginSignals();
    const extracted = structuredText();
    const rawStructured = extracted.structured || "";
    const rawProse = extracted.prose || "";
    const totalRaw = rawStructured.length + rawProse.length;
    const structured = rawStructured.slice(0, READ_POOL);
    const prose = rawProse.slice(0, READ_POOL);
    const sep = structured && prose ? "\n\n" : "";
    const combined = structured + sep + prose;
    const offset = Math.max(0, Number(message.offset) || 0);
    const end = Math.min(combined.length, offset + READ_CHUNK);
    const windowText = combined.slice(offset, end);
    const structuredEnd = structured.length;
    let text = "";
    let proseOut = "";
    if (offset < structuredEnd) {
      text = combined.slice(offset, Math.min(end, structuredEnd));
      if (end > structuredEnd + sep.length) {
        proseOut = combined.slice(structuredEnd + sep.length, end);
      }
    } else {
      proseOut = windowText;
    }
    return {
      url: location.href,
      title: document.title,
      text,
      prose: proseOut,
      offset,
      nextOffset: end,
      hasMore: end < combined.length,
      truncated: rawStructured.length > READ_POOL || rawProse.length > READ_POOL,
      totalChars: totalRaw,
      ...signals,
    };
  };

  const scrollPage = (message) => {
    if (message.text) {
      const needle = String(message.text).toLowerCase();
      const match = deepQueryAll("h1,h2,h3,h4,td,th,p,span,li,a,div,button,[role='row'],[role='gridcell']")
        .filter(visible)
        .find((element) => (element.innerText || "").toLowerCase().includes(needle));
      if (!match) throw new Error(`No visible text matches "${String(message.text).slice(0, 80)}".`);
      match.scrollIntoView({ block: "center" });
      return { scrolled: true, matched: String(message.text).slice(0, 120) };
    }
    const amount = Math.max(200, Math.floor(window.innerHeight * 0.85));
    const before = window.scrollY;
    window.scrollBy(0, message.direction === "up" ? -amount : amount);
    const max = Math.max(0, document.documentElement.scrollHeight - window.innerHeight);
    return {
      scrolled: window.scrollY !== before,
      scrollY: Math.round(window.scrollY),
      maxScroll: Math.round(max),
      atStart: window.scrollY <= 2,
      atEnd: window.scrollY >= max - 2,
    };
  };

  const INTERACTIVE =
    "a[href],button,input,textarea,select,summary,[contenteditable=true],[contenteditable=''],[role=button],[role=link],[role=tab],[role=menuitem],[role=menuitemcheckbox],[role=option],[role=row],[role=gridcell],[role=checkbox],[role=switch],[role=combobox],[role=textbox],[role=searchbox],[role=slider]";

  const fingerprint = (element) => ({
    tag: element.tagName.toLowerCase(),
    role: element.getAttribute("role") || "",
    type: element.getAttribute("type") || "",
    name: element.getAttribute("name") || "",
    id: element.id || "",
    aria: element.getAttribute("aria-label") || "",
    href: (element.getAttribute("href") || "").slice(0, 160),
    label: label(element).slice(0, 160),
  });

  const sameFingerprint = (left, right) =>
    left.tag === right.tag &&
    left.role === right.role &&
    left.type === right.type &&
    left.name === right.name &&
    left.id === right.id &&
    left.aria === right.aria &&
    left.href === right.href &&
    left.label === right.label;

  const remember = (ref, element) => {
    refs.set(ref, { element, fingerprint: fingerprint(element) });
  };

  const observe = () => {
    const previous = new Map(refs);
    refs.clear();
    const elements = deepQueryAll(INTERACTIVE).filter(visible).slice(0, 500);
    const signals = loginSignals();
    return {
      url: location.href,
      title: document.title,
      ...signals,
      elements: elements.map((element, index) => {
        const ref = `e${index + 1}`;
        remember(ref, element);
        const rect = element.getBoundingClientRect();
        return {
          ref,
          tag: element.tagName.toLowerCase(),
          role: element.getAttribute("role"),
          label: label(element).slice(0, 300),
          type: element.getAttribute("type"),
          disabled: element.matches(":disabled"),
          bounds: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
        };
      }),
      retained: [...previous.keys()].filter((ref) => refs.has(ref)).length,
    };
  };

  const findByText = (message) => {
    const needle = String(message.text || "").trim().toLowerCase();
    if (!needle) throw new Error("find requires a text string.");
    observe();
    const matches = [];
    for (const [ref, entry] of refs.entries()) {
      const element = entry.element;
      const lab = label(element).toLowerCase();
      const inner = (element.innerText || "").toLowerCase().replace(/\s+/g, " ");
      if (lab.includes(needle) || inner.includes(needle)) {
        matches.push({
          ref,
          tag: element.tagName.toLowerCase(),
          role: element.getAttribute("role"),
          label: label(element).slice(0, 160),
        });
      }
    }
    if (matches.length < 10) {
      const extras = deepQueryAll("h1,h2,h3,h4,td,th,p,li,span,div,[role='row']")
        .filter(visible)
        .filter((element) => (element.innerText || "").toLowerCase().includes(needle))
        .slice(0, 15);
      const known = [...refs.values()].map((entry) => entry.element);
      for (const element of extras) {
        if (known.includes(element)) continue;
        const ref = `t${refs.size + 1}`;
        remember(ref, element);
        matches.push({
          ref,
          tag: element.tagName.toLowerCase(),
          role: element.getAttribute("role"),
          label: (element.innerText || "").trim().slice(0, 160),
        });
      }
    }
    return { text: message.text, count: matches.length, matches: matches.slice(0, 20) };
  };

  const waitFor = (message) => {
    const needle = String(message.text || "").trim().toLowerCase();
    if (!needle) throw new Error("wait requires a text string.");
    const timeoutMs = Math.min(30000, Math.max(500, Number(message.timeoutMs) || 12000));
    const started = Date.now();
    const poll = () => {
      const body = frameDocuments()
        .map((doc) => doc.body?.innerText || "")
        .join("\n")
        .toLowerCase();
      const href = location.href.toLowerCase();
      if (body.includes(needle) || href.includes(needle)) {
        return {
          ready: true,
          matched: String(message.text).slice(0, 120),
          url: location.href,
          waitedMs: Date.now() - started,
        };
      }
      if (Date.now() - started >= timeoutMs) {
        throw new Error(`Timed out after ${timeoutMs}ms waiting for "${String(message.text).slice(0, 80)}".`);
      }
      return new Promise((resolve, reject) => {
        setTimeout(() => {
          try {
            resolve(poll());
          } catch (error) {
            reject(error);
          }
        }, 300);
      });
    };
    return poll();
  };

  const elementFor = (ref) => {
    const entry = refs.get(ref);
    if (!entry) throw new Error("Element reference expired; observe or find the page again.");
    if (entry.element?.isConnected && sameFingerprint(fingerprint(entry.element), entry.fingerprint)) {
      return entry.element;
    }
    const candidates = deepQueryAll(INTERACTIVE)
      .filter(visible)
      .filter((element) => sameFingerprint(fingerprint(element), entry.fingerprint));
    if (candidates.length === 1) {
      entry.element = candidates[0];
      return candidates[0];
    }
    throw new Error("Element reference expired; observe or find the page again.");
  };

  const viewportBox = (element) => {
    const view = ownerView(element);
    const rect = element.getBoundingClientRect();
    const width = Math.max(1, view?.innerWidth || 0);
    const height = Math.max(1, view?.innerHeight || 0);
    const nx = (rect.x + rect.width / 2) / width;
    const ny = (rect.y + rect.height / 2) / height;
    if (!Number.isFinite(nx) || !Number.isFinite(ny) || nx < 0 || nx > 1 || ny < 0 || ny > 1) {
      return null;
    }
    return { nx: Number(nx.toFixed(4)), ny: Number(ny.toFixed(4)), space: "page" };
  };

  const isContentEditable = (element) => {
    const value = element.getAttribute?.("contenteditable");
    return element.isContentEditable || value === "true" || value === "";
  };

  const prefersTrustedInput = (element) => {
    const role = (element.getAttribute("role") || "").toLowerCase();
    const tag = (element.tagName || "").toLowerCase();
    return isContentEditable(element) || role === "textbox" || tag === "canvas";
  };

  const withBox = (result, element) => {
    const box = viewportBox(element);
    return box ? { ...result, ...box } : result;
  };

  const pointerOpts = (element, extra = {}) => {
    const rect = element.getBoundingClientRect();
    return {
      bubbles: true,
      cancelable: true,
      view: ownerView(element),
      button: 0,
      clientX: rect.x + rect.width / 2,
      clientY: rect.y + rect.height / 2,
      ...extra,
    };
  };

  const dispatchPointer = (element, type, extra = {}) => {
    const opts = pointerOpts(element, extra);
    const view = ownerView(element);
    const Pointer = view.PointerEvent || PointerEvent;
    const Mouse = view.MouseEvent || MouseEvent;
    const pointerType = type.startsWith("mouse") ? type.replace("mouse", "pointer") : null;
    if (pointerType) element.dispatchEvent(new Pointer(pointerType, opts));
    element.dispatchEvent(new Mouse(type, opts));
  };

  const requireEnabled = (element) => {
    if (element.disabled || element.getAttribute("aria-disabled") === "true") {
      throw new Error("The target control is disabled.");
    }
  };

  const clickElement = (element) => {
    element.scrollIntoView({ block: "center", inline: "nearest" });
    requireEnabled(element);
    if (prefersTrustedInput(element)) {
      return withBox({ clicked: false, needsTrustedInput: true, reason: "contenteditable" }, element);
    }
    dispatchPointer(element, "mousedown", { detail: 1 });
    dispatchPointer(element, "mouseup", { detail: 1 });
    element.click();
    return withBox({ clicked: true, untrusted: true }, element);
  };

  const doubleClickElement = (element) => {
    element.scrollIntoView({ block: "center", inline: "nearest" });
    requireEnabled(element);
    if (prefersTrustedInput(element)) {
      return withBox({ dblclicked: false, needsTrustedInput: true, reason: "contenteditable" }, element);
    }
    dispatchPointer(element, "mousedown", { detail: 1 });
    dispatchPointer(element, "mouseup", { detail: 1 });
    dispatchPointer(element, "click", { detail: 1 });
    dispatchPointer(element, "mousedown", { detail: 2 });
    dispatchPointer(element, "mouseup", { detail: 2 });
    dispatchPointer(element, "click", { detail: 2 });
    dispatchPointer(element, "dblclick", { detail: 2 });
    return withBox({ dblclicked: true, untrusted: true }, element);
  };

  const setNativeValue = (element, text) => {
    const view = element.ownerDocument?.defaultView || window;
    const TextArea = view.HTMLTextAreaElement;
    const Input = view.HTMLInputElement;
    const proto =
      TextArea && element instanceof TextArea
        ? TextArea.prototype
        : Input && element instanceof Input
          ? Input.prototype
          : null;
    const setter = proto ? Object.getOwnPropertyDescriptor(proto, "value")?.set : null;
    if (setter) setter.call(element, text);
    else if ("value" in element) element.value = text;
    else element.textContent = text;
  };

  const typeElement = (element, text) => {
    if (element.getAttribute("type") === "password") {
      throw new Error("Alfred never types into password fields; use the browser password manager.");
    }
    element.scrollIntoView({ block: "center", inline: "nearest" });
    element.focus();
    if (prefersTrustedInput(element)) {
      return withBox(
        { typed: false, verified: false, needsTrustedInput: true, reason: "contenteditable", characters: text.length },
        element
      );
    }
    setNativeValue(element, text);
    const view = ownerView(element);
    const Input = view.InputEvent || InputEvent;
    element.dispatchEvent(new Input("input", { bubbles: true, inputType: "insertText", data: text }));
    element.dispatchEvent(new view.Event("change", { bubbles: true }));
    const observed = "value" in element ? String(element.value || "") : element.innerText || "";
    if (text && !observed.includes(text) && observed !== text) {
      return withBox(
        { typed: false, verified: false, needsTrustedInput: true, reason: "unverified", characters: text.length },
        element
      );
    }
    return withBox({ typed: true, verified: true, characters: text.length }, element);
  };

  chrome.runtime.onMessage.addListener((message, _sender, respond) => {
    Promise.resolve()
      .then(() => {
        if (message.method === "ping") return { pong: true, version: 3 };
        if (message.method === "observe") return observe();
        if (message.method === "read") return readPage(message);
        if (message.method === "scroll") return scrollPage(message);
        if (message.method === "find") return findByText(message);
        if (message.method === "wait") return waitFor(message);
        const element = elementFor(message.ref);
        const liveLabel = [label(element), element.getAttribute("aria-label"), element.innerText]
          .filter(Boolean)
          .join(" ");
        const context = ancestorContext(element);
        const blocked =
          message.method !== "getText" &&
          message.method !== "type" &&
          !isDismissiveLabel(label(element)) &&
          (isDestructionLabel(liveLabel) ||
            (isConfirmationLabel(label(element)) && isDestructionLabel(context)));
        if (blocked) {
          throw new Error("Destructive browser actions are blocked by Alfred.");
        }
        if (message.method === "click") return { ...clickElement(element), label: label(element).slice(0, 160) };
        if (message.method === "dblclick") {
          return { ...doubleClickElement(element), label: label(element).slice(0, 160) };
        }
        if (message.method === "hover") {
          element.scrollIntoView({ block: "center" });
          const view = ownerView(element);
          const Pointer = view.PointerEvent || PointerEvent;
          const Mouse = view.MouseEvent || MouseEvent;
          dispatchPointer(element, "mouseover");
          element.dispatchEvent(new Pointer("pointerenter", { bubbles: true, view }));
          element.dispatchEvent(new Mouse("mouseenter", { bubbles: true, view }));
          dispatchPointer(element, "mousemove");
          return withBox({ hovered: true, untrusted: true }, element);
        }
        if (message.method === "type") return typeElement(element, String(message.text ?? ""));
        if (message.method === "getText") {
          const text = (("value" in element ? element.value : element.innerText?.trim()) || label(element)).slice(
            0,
            2000
          );
          return { text };
        }
        throw new Error(`Unsupported browser method: ${message.method}`);
      })
      .then(respond)
      .catch((error) => respond({ error: String(error.message ?? error) }));
    return true;
  });
})();
