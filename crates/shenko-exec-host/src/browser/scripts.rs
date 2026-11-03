//! Page-side scripts executed inside the real page.
//!
//! These run in the browser's own JavaScript context and report what the
//! page itself says. The adapter never fabricates a target, a coordinate or
//! a text node: when the page cannot answer, the observation says so.

/// The observation script: location, title, visible text and the structured
/// targets a worker can act on.
pub fn snapshot_script() -> String {
    r##"(function () {
  const MAX_TEXT = 65536;
  const MAX_TARGETS = 256;
  const text = (document.body && document.body.innerText) || "";
  const truncated = text.length > MAX_TEXT;
  const visible = (el) => {
    if (!el) return false;
    const rect = el.getBoundingClientRect();
    const style = window.getComputedStyle(el);
    return rect.width > 0 && rect.height > 0 && style.visibility !== "hidden" && style.display !== "none";
  };
  const rectOf = (el) => {
    if (!el) return null;
    const rect = el.getBoundingClientRect();
    return [rect.x, rect.y, rect.width, rect.height];
  };
  const describe = (el, role) => {
    let selector = el.tagName.toLowerCase();
    if (el.id) selector += "#" + el.id;
    if (el.getAttribute("name")) selector += "[name=" + JSON.stringify(el.getAttribute("name")) + "]";
    return {
      role: role,
      name: (el.getAttribute("aria-label") || el.innerText || el.value || el.getAttribute("title") || "").trim().slice(0, 200),
      selector: selector,
      href: el.tagName === "A" ? (el.getAttribute("href") || null) : null,
      input_type: el.tagName === "INPUT" ? (el.getAttribute("type") || "text") : null,
      value_len: (el.tagName === "INPUT" || el.tagName === "TEXTAREA") ? String(el.value || "").length : null,
      visible: visible(el),
      rect: rectOf(el)
    };
  };
  const links = Array.from(document.querySelectorAll("a[href]")).slice(0, MAX_TARGETS).map((el) => describe(el, "link"));
  const inputs = Array.from(document.querySelectorAll("input, textarea, select")).slice(0, MAX_TARGETS).map((el) => describe(el, "input"));
  const buttons = Array.from(document.querySelectorAll("button, input[type=submit], input[type=button], [role=button]")).slice(0, MAX_TARGETS).map((el) => describe(el, "button"));
  return JSON.stringify({
    url: location.href,
    title: document.title || "",
    readyState: document.readyState,
    text: text.slice(0, MAX_TEXT),
    textTruncated: truncated,
    viewport: {
      width: window.innerWidth,
      height: window.innerHeight,
      deviceScaleFactor: window.devicePixelRatio || 1
    },
    links: links,
    inputs: inputs,
    buttons: buttons
  });
})()"##
    .to_string()
}

/// Click the element matched by a CSS selector, or by its visible text.
pub fn click_script(selector: Option<&str>, text: Option<&str>) -> String {
    let selector = serde_json::to_string(&selector.map(str::to_string)).unwrap_or_else(|_| "null".into());
    let text = serde_json::to_string(&text.map(str::to_string)).unwrap_or_else(|_| "null".into());
    format!(
        r#"(function () {{
  const selector = {selector};
  const wanted = {text};
  let el = null;
  if (selector) {{
    el = document.querySelector(selector);
  }} else if (wanted) {{
    const needle = wanted.trim().toLowerCase();
    const candidates = Array.from(document.querySelectorAll("a, button, input[type=submit], input[type=button], [role=button], [onclick]"));
    el = candidates.find((candidate) => {{
      const label = (candidate.innerText || candidate.value || candidate.getAttribute("aria-label") || "").trim().toLowerCase();
      return label === needle || label.includes(needle);
    }}) || null;
  }}
  if (!el) return JSON.stringify({{found: false}});
  el.scrollIntoView({{block: "center", inline: "center"}});
  const rect = el.getBoundingClientRect();
  el.click();
  return JSON.stringify({{found: true, tag: el.tagName.toLowerCase(), rect: [rect.x, rect.y, rect.width, rect.height]}});
}})()"#
    )
}

/// Set a field's value through the native setter so frameworks that watch the
/// property still see the change, then dispatch the real input/change events.
pub fn fill_script(selector: &str, value: &str, submit: bool) -> String {
    let selector = serde_json::to_string(selector).unwrap_or_default();
    let value = serde_json::to_string(value).unwrap_or_default();
    format!(
        r#"(function () {{
  const selector = {selector};
  const el = document.querySelector(selector);
  if (!el) return JSON.stringify({{found: false}});
  el.scrollIntoView({{block: "center"}});
  el.focus();
  const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
  const descriptor = Object.getOwnPropertyDescriptor(proto, "value");
  if (descriptor && descriptor.set) descriptor.set.call(el, {value});
  else el.value = {value};
  el.dispatchEvent(new Event("input", {{bubbles: true}}));
  el.dispatchEvent(new Event("change", {{bubbles: true}}));
  let submitted = false;
  if ({submit}) {{
    const form = el.form || el.closest("form");
    if (form) {{
      submitted = true;
      if (typeof form.requestSubmit === "function") form.requestSubmit();
      else form.submit();
    }}
  }}
  return JSON.stringify({{found: true, submitted: submitted, valueLen: String(el.value || "").length}});
}})()"#
    )
}

/// Report whether a selector currently matches.
pub fn exists_script(selector: &str) -> String {
    let selector = serde_json::to_string(selector).unwrap_or_default();
    format!(
        r#"(function () {{
  const el = document.querySelector({selector});
  if (!el) return JSON.stringify({{found: false}});
  const rect = el.getBoundingClientRect();
  return JSON.stringify({{found: true, rect: [rect.x, rect.y, rect.width, rect.height], text: (el.innerText || "").slice(0, 200)}});
}})()"#
    )
}

/// Export the origin-scoped localStorage of the current page.
pub fn local_storage_script() -> String {
    r#"(function () {
  const out = {};
  try {
    for (let i = 0; i < localStorage.length; i += 1) {
      const key = localStorage.key(i);
      out[key] = localStorage.getItem(key);
    }
  } catch (error) {
    return JSON.stringify({ok: false, error: String(error)});
  }
  return JSON.stringify({ok: true, origin: location.origin, items: out});
})()"#
        .to_string()
}

/// Import a localStorage state into the current origin.
pub fn set_local_storage_script(items: &serde_json::Map<String, serde_json::Value>) -> String {
    let payload = serde_json::to_string(items).unwrap_or_else(|_| "{}".into());
    format!(
        r#"(function () {{
  const items = {payload};
  try {{
    for (const key of Object.keys(items)) localStorage.setItem(key, items[key]);
    return JSON.stringify({{ok: true, count: Object.keys(items).length}});
  }} catch (error) {{
    return JSON.stringify({{ok: false, error: String(error)}});
  }}
}})()"#
    )
}


