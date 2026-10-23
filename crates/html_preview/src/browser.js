// Runs at the start of every page Bench's browser loads. It reports the
// page's console, errors, failed requests and route changes to Bench, and
// gives agents `window.__bench`: a snapshot of the page's elements with refs,
// and helpers the browser tools use to click and type into them.
(() => {
  if (window.__bench) {
    return;
  }
  const ipc = window.ipc;
  const post = (message) => {
    try {
      ipc.postMessage(JSON.stringify(message));
    } catch {}
  };

  const MAX_MESSAGE_LENGTH = 4000;

  const describe = (value) => {
    if (typeof value === "string") {
      return value;
    }
    if (value instanceof Error) {
      const message = `${value.name}: ${value.message}`;
      return value.stack ? `${message}\n${value.stack}` : message;
    }
    if (value instanceof Element) {
      return `<${value.localName}${value.id ? `#${value.id}` : ""}>`;
    }
    if (typeof value === "function") {
      return `[function ${value.name || "anonymous"}]`;
    }
    if (value === undefined) {
      return "undefined";
    }
    try {
      return JSON.stringify(value) ?? String(value);
    } catch {
      return String(value);
    }
  };

  const report = (level, parts) => {
    let text = parts.map(describe).join(" ");
    if (text.length > MAX_MESSAGE_LENGTH) {
      text = `${text.slice(0, MAX_MESSAGE_LENGTH)}…`;
    }
    post({ type: "console", level, text });
  };

  for (const level of ["log", "info", "warn", "error", "debug"]) {
    const original = console[level];
    console[level] = function (...parts) {
      report(level, parts);
      return original.apply(this, parts);
    };
  }

  window.addEventListener(
    "error",
    (event) => {
      const target = event.target;
      if (target && target !== window) {
        const url = target.currentSrc || target.src || target.href;
        if (url) {
          report("error", [`Failed to load ${target.localName}: ${url}`]);
        }
        return;
      }
      // A script that fails to parse when an agent runs it is reported
      // with no details, and isn't the page's error.
      if (event.message === "Script error." && !event.filename) {
        return;
      }
      if (event.error instanceof Error) {
        report("error", ["Uncaught", event.error]);
      } else {
        const where = event.filename
          ? ` (${event.filename}:${event.lineno}:${event.colno})`
          : "";
        report("error", [`Uncaught ${event.message}${where}`]);
      }
    },
    true,
  );
  window.addEventListener("unhandledrejection", (event) => {
    report("error", ["Unhandled promise rejection:", event.reason]);
  });

  const originalFetch = window.fetch;
  if (originalFetch) {
    window.fetch = async function (input, init) {
      const method = String(init?.method ?? input?.method ?? "GET").toUpperCase();
      const url = typeof input === "string" ? input : (input?.url ?? String(input));
      try {
        const response = await originalFetch.call(window, input, init);
        if (!response.ok) {
          report("network", [
            `${method} ${response.url || url} → ${response.status} ${response.statusText}`,
          ]);
        }
        return response;
      } catch (error) {
        report("network", [`${method} ${url} failed: ${describe(error)}`]);
        throw error;
      }
    };
  }

  const xhr = window.XMLHttpRequest?.prototype;
  if (xhr) {
    const open = xhr.open;
    xhr.open = function (method, url, ...rest) {
      this.__benchRequest = `${String(method).toUpperCase()} ${url}`;
      return open.call(this, method, url, ...rest);
    };
    const send = xhr.send;
    xhr.send = function (...args) {
      this.addEventListener("loadend", () => {
        if (this.status === 0) {
          report("network", [`${this.__benchRequest} failed or was cancelled`]);
        } else if (this.status >= 400) {
          report("network", [`${this.__benchRequest} → ${this.status} ${this.statusText}`]);
        }
      });
      return send.apply(this, args);
    };
  }

  const reportLocation = () => post({ type: "location", url: location.href });
  for (const name of ["pushState", "replaceState"]) {
    const original = history[name];
    history[name] = function (...args) {
      const result = original.apply(this, args);
      reportLocation();
      return result;
    };
  }
  window.addEventListener("popstate", reportLocation);
  window.addEventListener("hashchange", reportLocation);

  // Snapshots.

  const MAX_SNAPSHOT_LINES = 2000;
  const MAX_NAME_LENGTH = 100;
  const MAX_TEXT_LENGTH = 200;

  const INTERACTIVE_ROLES = new Set([
    "button",
    "checkbox",
    "combobox",
    "link",
    "listbox",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "radio",
    "searchbox",
    "slider",
    "spinbutton",
    "switch",
    "tab",
    "textbox",
    "treeitem",
  ]);
  const STRUCTURAL_ROLES = new Set([
    "alert",
    "dialog",
    "form",
    "heading",
    "img",
    "list",
    "listitem",
    "main",
    "navigation",
    "row",
    "table",
    "tablist",
    "tabpanel",
  ]);
  const INPUT_ROLES = {
    button: "button",
    checkbox: "checkbox",
    color: "button",
    file: "button",
    image: "button",
    number: "spinbutton",
    radio: "radio",
    range: "slider",
    reset: "button",
    search: "searchbox",
    submit: "button",
  };

  const implicitRole = (element) => {
    switch (element.localName) {
      case "a":
      case "area":
        return element.hasAttribute("href") ? "link" : null;
      case "button":
      case "summary":
        return "button";
      case "input": {
        const type = (element.getAttribute("type") ?? "text").toLowerCase();
        return type === "hidden" ? null : (INPUT_ROLES[type] ?? "textbox");
      }
      case "textarea":
        return "textbox";
      case "select":
        return element.multiple || element.size > 1 ? "listbox" : "combobox";
      case "option":
        return "option";
      case "img":
        return element.getAttribute("alt") === "" ? null : "img";
      case "h1":
      case "h2":
      case "h3":
      case "h4":
      case "h5":
      case "h6":
        return "heading";
      case "nav":
        return "navigation";
      case "main":
        return "main";
      case "dialog":
        return "dialog";
      case "form":
        return "form";
      case "ul":
      case "ol":
        return "list";
      case "li":
        return "listitem";
      case "table":
        return "table";
      case "tr":
        return "row";
      default:
        return element.isContentEditable &&
          !element.parentElement?.isContentEditable
          ? "textbox"
          : null;
    }
  };

  const roleOf = (element) => {
    const explicit = element.getAttribute("role")?.trim().split(/\s+/)[0];
    if (explicit && explicit !== "none" && explicit !== "presentation") {
      return explicit;
    }
    const role = implicitRole(element);
    if (role) {
      return role;
    }
    if (
      element.hasAttribute("onclick") ||
      (element.tabIndex >= 0 && element.hasAttribute("tabindex"))
    ) {
      return "generic-clickable";
    }
    return null;
  };

  const clip = (text, length) => {
    const collapsed = text.replace(/\s+/g, " ").trim();
    return collapsed.length > length ? `${collapsed.slice(0, length)}…` : collapsed;
  };

  // Roles named by their own text, which is then not listed beneath them.
  const NAMED_BY_CONTENT = new Set([
    ...INTERACTIVE_ROLES,
    "generic-clickable",
    "heading",
  ]);

  const nameOf = (element, role) => {
    const label = element.getAttribute("aria-label");
    if (label?.trim()) {
      return clip(label, MAX_NAME_LENGTH);
    }
    const labelledBy = element.getAttribute("aria-labelledby");
    if (labelledBy) {
      const text = labelledBy
        .split(/\s+/)
        .map((id) => element.getRootNode().getElementById?.(id)?.textContent ?? "")
        .join(" ");
      if (text.trim()) {
        return clip(text, MAX_NAME_LENGTH);
      }
    }
    if (element.labels?.length) {
      const text = Array.from(element.labels, (label) => label.textContent).join(" ");
      if (text.trim()) {
        return clip(text, MAX_NAME_LENGTH);
      }
    }
    if (element.localName === "img" || element.localName === "area") {
      return clip(element.getAttribute("alt") ?? "", MAX_NAME_LENGTH);
    }
    if (element.localName === "input" || element.localName === "textarea") {
      const type = element.getAttribute("type");
      if (type === "submit" || type === "button" || type === "reset") {
        return clip(element.value, MAX_NAME_LENGTH);
      }
      return clip(element.getAttribute("placeholder") ?? element.title ?? "", MAX_NAME_LENGTH);
    }
    if (element.localName === "select") {
      return clip(element.title ?? "", MAX_NAME_LENGTH);
    }
    if (role && !NAMED_BY_CONTENT.has(role)) {
      return clip(element.title ?? "", MAX_NAME_LENGTH);
    }
    const text = element.innerText ?? element.textContent ?? "";
    return clip(text || element.title || "", MAX_NAME_LENGTH);
  };

  const isHidden = (element) => {
    if (element.getAttribute("aria-hidden") === "true") {
      return true;
    }
    if (element.checkVisibility) {
      return !element.checkVisibility({ checkOpacity: false, checkVisibilityCSS: true });
    }
    const style = getComputedStyle(element);
    return style.display === "none" || style.visibility === "hidden";
  };

  const refs = new Map();
  const refOfElement = new WeakMap();
  let nextRef = 1;

  const refFor = (element) => {
    let ref = refOfElement.get(element);
    if (!ref) {
      ref = `e${nextRef++}`;
      refOfElement.set(element, ref);
    }
    refs.set(ref, new WeakRef(element));
    return ref;
  };

  const states = (element, role) => {
    const parts = [];
    if (role === "heading") {
      const level =
        element.getAttribute("aria-level") ?? /^h([1-6])$/.exec(element.localName)?.[1];
      if (level) {
        parts.push(`[level=${level}]`);
      }
    }
    const checked = element.getAttribute("aria-checked") ?? (element.checked ? "true" : null);
    if (checked === "true") {
      parts.push("[checked]");
    } else if (checked === "mixed") {
      parts.push("[mixed]");
    }
    if (element.disabled || element.getAttribute("aria-disabled") === "true") {
      parts.push("[disabled]");
    }
    if (element.getAttribute("aria-expanded") === "true") {
      parts.push("[expanded]");
    } else if (element.getAttribute("aria-expanded") === "false") {
      parts.push("[collapsed]");
    }
    if (
      element.getAttribute("aria-selected") === "true" ||
      (element.localName === "option" && element.selected)
    ) {
      parts.push("[selected]");
    }
    if (element === deepActiveElement()) {
      parts.push("[focused]");
    }
    if (role === "link") {
      const href = element.getAttribute("href");
      if (href) {
        parts.push(`-> ${clip(href, MAX_NAME_LENGTH)}`);
      }
    }
    if ("value" in element && ["textbox", "searchbox", "combobox", "spinbutton", "slider"].includes(role)) {
      const value = element.type === "password" ? "•••" : clip(String(element.value), MAX_TEXT_LENGTH);
      if (value) {
        parts.push(`value="${value}"`);
      }
    } else if (element.isContentEditable && role === "textbox") {
      const value = clip(element.innerText ?? "", MAX_TEXT_LENGTH);
      if (value) {
        parts.push(`value="${value}"`);
      }
    }
    return parts;
  };

  // The nodes a node renders: its shadow tree's, a slot's assigned nodes, or
  // its own children.
  const renderedChildren = (node) => {
    if (node.shadowRoot) {
      return node.shadowRoot.childNodes;
    }
    if (node.localName === "slot") {
      const assigned = node.assignedNodes({ flatten: true });
      return assigned.length ? assigned : node.childNodes;
    }
    return node.childNodes;
  };

  const SKIPPED_ELEMENTS = new Set(["script", "style", "noscript", "template", "head", "meta", "link"]);

  const snapshot = () => {
    const lines = [];
    let truncated = false;
    const emit = (depth, text) => {
      if (lines.length >= MAX_SNAPSHOT_LINES) {
        truncated = true;
        return;
      }
      lines.push(`${"  ".repeat(depth)}- ${text}`);
    };

    const visit = (node, depth, insideNamedElement) => {
      if (truncated) {
        return;
      }
      if (node.nodeType === Node.TEXT_NODE) {
        if (!insideNamedElement) {
          const text = clip(node.textContent ?? "", MAX_TEXT_LENGTH);
          if (text) {
            emit(depth, `text: ${text}`);
          }
        }
        return;
      }
      if (node.nodeType !== Node.ELEMENT_NODE && node.nodeType !== Node.DOCUMENT_FRAGMENT_NODE) {
        return;
      }
      if (node.nodeType === Node.ELEMENT_NODE) {
        if (SKIPPED_ELEMENTS.has(node.localName) || isHidden(node)) {
          return;
        }
        if (node.localName === "iframe") {
          emit(depth, `iframe ${clip(node.getAttribute("src") ?? "", MAX_NAME_LENGTH)} (not included)`);
          return;
        }
        const role = roleOf(node);
        if (role) {
          const interactive = INTERACTIVE_ROLES.has(role) || role === "generic-clickable";
          if (interactive || STRUCTURAL_ROLES.has(role) || node.getAttribute("role")) {
            const name = nameOf(node, role);
            const shownRole = role === "generic-clickable" ? "clickable" : role;
            const parts = [name ? `${shownRole} "${name.replaceAll('"', '\\"')}"` : shownRole];
            if (interactive) {
              parts.push(`[ref=${refFor(node)}]`);
            }
            parts.push(...states(node, role));
            emit(depth, parts.join(" "));
            // An element named by its text doesn't repeat it beneath it;
            // elements inside it are still listed.
            const namedByContent = NAMED_BY_CONTENT.has(role);
            for (const child of renderedChildren(node)) {
              visit(child, depth + 1, insideNamedElement || namedByContent);
            }
            return;
          }
        }
      }
      for (const child of renderedChildren(node)) {
        visit(child, depth, insideNamedElement);
      }
    };

    if (document.body) {
      visit(document.body, 0, false);
    }
    if (truncated) {
      lines.push(`… cut off at ${MAX_SNAPSHOT_LINES} lines`);
    }
    return lines.join("\n");
  };

  // Flutter.

  const isFlutter = () => !!document.querySelector("flutter-view, flt-glass-pane");

  // Flutter draws to a canvas and only builds elements for its semantics
  // tree once accessibility is turned on, which a click on its placeholder
  // button does.
  const enableFlutterSemantics = async () => {
    if (!isFlutter() || deepQuerySelector("flt-semantics")) {
      return false;
    }
    const placeholder = deepQuerySelector("flt-semantics-placeholder");
    if (!placeholder) {
      return false;
    }
    placeholder.click();
    await new Promise((resolve) => setTimeout(resolve, 300));
    return true;
  };

  // Elements.

  const deepQuerySelector = (selector, root = document) => {
    const found = root.querySelector(selector);
    if (found) {
      return found;
    }
    for (const element of root.querySelectorAll("*")) {
      if (element.shadowRoot) {
        const nested = deepQuerySelector(selector, element.shadowRoot);
        if (nested) {
          return nested;
        }
      }
    }
    return null;
  };

  const deepActiveElement = () => {
    let element = document.activeElement;
    while (element?.shadowRoot?.activeElement) {
      element = element.shadowRoot.activeElement;
    }
    return element;
  };

  const deepElementFromPoint = (x, y) => {
    let element = document.elementFromPoint(x, y);
    while (element?.shadowRoot) {
      const inner = element.shadowRoot.elementFromPoint(x, y);
      if (!inner || inner === element) {
        break;
      }
      element = inner;
    }
    return element;
  };

  const TEXT_INPUT_TYPES = new Set([
    "text",
    "search",
    "email",
    "url",
    "tel",
    "password",
    "number",
    "date",
    "datetime-local",
    "month",
    "time",
    "week",
  ]);

  const isEditable = (element) => {
    if (!element) {
      return false;
    }
    if (element.localName === "textarea") {
      return !element.readOnly && !element.disabled;
    }
    if (element.localName === "input") {
      const type = (element.getAttribute("type") ?? "text").toLowerCase();
      return TEXT_INPUT_TYPES.has(type) && !element.readOnly && !element.disabled;
    }
    return element.isContentEditable;
  };

  const resolve = (target) => {
    if (target.ref) {
      const element = refs.get(target.ref)?.deref();
      if (!element || !element.isConnected) {
        throw new Error(
          `${target.ref} is not on the page any more. Take a new snapshot to get current refs.`,
        );
      }
      return element;
    }
    const element = deepQuerySelector(target.selector);
    if (!element) {
      throw new Error(`Nothing on the page matches the selector ${target.selector}`);
    }
    return element;
  };

  const label = (element) => {
    const role = roleOf(element);
    const name = nameOf(element, role);
    const kind = role && role !== "generic-clickable" ? role : element.localName;
    return name ? `${kind} "${name}"` : kind;
  };

  // Scrolls the element into view and returns the point at its center that a
  // click should land on, relative to the page's top left corner.
  const clickPoint = async (target) => {
    const element = resolve(target);
    element.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
    await new Promise((resolve) => setTimeout(resolve, 50));
    const rect = element.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) {
      throw new Error(`${label(element)} has no size on the page, so it can't be clicked`);
    }
    const x = rect.left + rect.width / 2;
    const y = rect.top + rect.height / 2;
    // Flutter's semantics elements sit over its canvas and don't take
    // pointer events, so the canvas is always what's at the point.
    const inFlutterSemantics = !!element.closest("flt-semantics-host, flt-semantics");
    const hit = deepElementFromPoint(x, y);
    const covered =
      !inFlutterSemantics && hit && hit !== element && !element.contains(hit)
        ? label(hit)
        : null;
    return { x, y, element: label(element), covered };
  };

  const typeText = (text, clear) => {
    const element = deepActiveElement();
    if (!isEditable(element)) {
      throw new Error(
        "No text field has focus in the page. Click one first, or pass its `ref` or `selector`.",
      );
    }
    if (clear) {
      if (typeof element.select === "function") {
        element.select();
      } else {
        document.execCommand("selectAll");
      }
      document.execCommand("delete");
    }
    if (text && !document.execCommand("insertText", false, text) && "value" in element) {
      // Setting the value through the prototype's setter, rather than the
      // element's, is what makes frameworks such as React see the change.
      const setter = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(element), "value")?.set;
      setter?.call(element, element.value + text);
      element.dispatchEvent(
        new InputEvent("input", { bubbles: true, inputType: "insertText", data: text }),
      );
    }
    return { element: label(element), value: "value" in element ? element.value : element.innerText };
  };

  Object.defineProperty(window, "__bench", {
    value: Object.freeze({
      snapshot: async () => {
        const flutterSemanticsEnabled = await enableFlutterSemantics();
        return {
          url: location.href,
          title: document.title,
          flutter: isFlutter(),
          flutterSemanticsEnabled,
          snapshot: snapshot(),
        };
      },
      clickPoint,
      focusedEditable: () => isEditable(deepActiveElement()),
      typeText,
    }),
    enumerable: false,
  });
})();
