(() => {
  const refs = new Map();
  const destructive = /\b(delete|remove|erase|trash|purge|wipe|shred|empty\s+(trash|bin|recycle))\b/i;
  const visible = (element) => {
    const rect = element.getBoundingClientRect();
    const style = getComputedStyle(element);
    return rect.width > 0 && rect.height > 0 && style.visibility !== "hidden" && style.display !== "none";
  };
  const label = (element) =>
    element.getAttribute("aria-label") ||
    element.innerText?.trim() ||
    element.getAttribute("placeholder") ||
    element.getAttribute("title") ||
    element.getAttribute("name") ||
    "";

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
  const READ_POOL = 32000;

  const loginSignals = () => {
    const body = (document.body?.innerText || "").toLowerCase();
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

  // Structured extraction so dashboards (tables, error lists, headings) are
  // usable for analysis — plain body.innerText alone loses row structure.
  const structuredText = () => {
    const parts = [];
    const push = (chunk) => {
      const text = (chunk || "").trim();
      if (text) parts.push(text);
    };

    for (const heading of deepQueryAll("h1,h2,h3,h4,[role='heading']").filter(visible).slice(0, 40)) {
      push(`# ${label(heading).slice(0, 200)}`);
    }

    for (const table of deepQueryAll("table").filter(visible).slice(0, 12)) {
      const rows = [...table.querySelectorAll("tr")].slice(0, 40).map((row) =>
        [...row.querySelectorAll("th,td")]
          .map((cell) => (cell.innerText || "").trim().replace(/\s+/g, " ").slice(0, 120))
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
        .map((item) => `- ${(item.innerText || "").trim().replace(/\s+/g, " ").slice(0, 200)}`)
        .filter((line) => line.length > 3);
      if (items.length) push(`LIST:\n${items.join("\n")}`);
    }

    // Role-based grids (common in SPAs / design systems).
    for (const grid of deepQueryAll("[role='grid'], [role='table']").filter(visible).slice(0, 8)) {
      const cells = [...grid.querySelectorAll("[role='row']")]
        .slice(0, 40)
        .map((row) =>
          [...row.querySelectorAll("[role='gridcell'], [role='columnheader'], [role='cell']")]
            .map((cell) => (cell.innerText || "").trim().replace(/\s+/g, " ").slice(0, 100))
            .filter(Boolean)
            .join(" | ")
        )
        .filter(Boolean);
      if (cells.length) push(`GRID:\n${cells.join("\n")}`);
    }

    const roots = ["article", "main", "[role=main]", "#content", "#main"]
      .map((selector) => {
        try {
          return document.querySelector(selector);
        } catch {
          return null;
        }
      })
      .filter((element) => element && (element.innerText || "").trim().length > 80);
    const root =
      roots.sort((a, b) => (b.innerText || "").length - (a.innerText || "").length)[0] || document.body;
    let prose = (root?.innerText || "").trim();

    const shadowChunks = [];
    const collectShadow = (scope) => {
      for (const element of scope.querySelectorAll("*")) {
        if (!element.shadowRoot) continue;
        for (const child of element.shadowRoot.children) {
          const chunk = (child.innerText || "").trim();
          if (chunk && !prose.includes(chunk.slice(0, 80))) shadowChunks.push(chunk);
        }
        collectShadow(element.shadowRoot);
      }
    };
    collectShadow(document);
    if (shadowChunks.length) prose += "\n" + shadowChunks.join("\n");

    push(prose);
    return parts.join("\n\n").replace(/\n{3,}/g, "\n\n");
  };

  const readPage = (message) => {
    const signals = loginSignals();
    let text = structuredText();
    const totalChars = text.length;
    const pooled = text.slice(0, READ_POOL);
    const offset = Math.max(0, Number(message.offset) || 0);
    const chunkText = pooled.slice(offset, offset + READ_CHUNK);
    return {
      url: location.href,
      title: document.title,
      text: chunkText,
      offset,
      nextOffset: offset + chunkText.length,
      hasMore: offset + chunkText.length < pooled.length,
      truncated: totalChars > pooled.length,
      totalChars,
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
    "a[href],button,input,textarea,select,[role=button],[role=link],[role=tab],[role=menuitem],[role=option],[role=row],[role=gridcell],[contenteditable=true]";

  const observe = () => {
    refs.clear();
    const elements = deepQueryAll(INTERACTIVE).filter(visible).slice(0, 500);
    const signals = loginSignals();
    return {
      url: location.href,
      title: document.title,
      ...signals,
      elements: elements.map((element, index) => {
        const ref = `e${index + 1}`;
        refs.set(ref, element);
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
    };
  };

  // Playwright-style getByText: rebuild refs, return matching interactive +
  // text-bearing nodes so the planner can click without re-scanning labels.
  const findByText = (message) => {
    const needle = String(message.text || "").trim().toLowerCase();
    if (!needle) throw new Error("find requires a text string.");
    observe();
    const matches = [];
    for (const [ref, element] of refs.entries()) {
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
    // Also index visible text nodes that are not interactive (for scroll/read targets).
    if (matches.length < 10) {
      const extras = deepQueryAll("h1,h2,h3,h4,td,th,p,li,span,div,[role='row']")
        .filter(visible)
        .filter((element) => (element.innerText || "").toLowerCase().includes(needle))
        .slice(0, 15);
      for (const element of extras) {
        if ([...refs.values()].includes(element)) continue;
        const ref = `t${refs.size + 1}`;
        refs.set(ref, element);
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
      const body = (document.body?.innerText || "").toLowerCase();
      if (body.includes(needle)) {
        return {
          ready: true,
          matched: String(message.text).slice(0, 120),
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

  const elementFor = (ref) =>
    refs.get(ref) ||
    (() => {
      throw new Error("Element reference expired; observe or find the page again.");
    })();

  chrome.runtime.onMessage.addListener((message, _sender, respond) => {
    Promise.resolve()
      .then(() => {
        if (message.method === "observe") return observe();
        // Read-only methods sit before the destructive-language gate so a search
        // string like "deleted errors" can never trip it; neither can change page
        // state (scroll is treated as observe-class navigation of the viewport).
        if (message.method === "read") return readPage(message);
        if (message.method === "scroll") return scrollPage(message);
        if (message.method === "find") return findByText(message);
        if (message.method === "wait") return waitFor(message);
        if (destructive.test(JSON.stringify(message))) {
          throw new Error("Destructive browser actions are blocked by Alfred.");
        }
        const element = elementFor(message.ref);
        if (message.method === "click") {
          element.scrollIntoView({ block: "center" });
          element.click();
          return { clicked: true };
        }
        if (message.method === "type") {
          if (element.getAttribute("type") === "password") {
            throw new Error("Alfred never types into password fields; use the browser password manager.");
          }
          element.focus();
          if ("value" in element) element.value = message.text;
          else element.textContent = message.text;
          element.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: message.text }));
          element.dispatchEvent(new Event("change", { bubbles: true }));
          return { typed: true, characters: message.text.length };
        }
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
