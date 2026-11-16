(() => {
  "use strict";
  const T = window.__TAURI__;
  const invoke = T.core.invoke;
  const $ = (id) => document.getElementById(id);

  // key in settings, i18n key, default value
  const OPTS = [
    ["recursive", "opt.recursive", true],
    ["autoScan", "opt.autoScan", true],
    ["backup", "opt.backup", true],
    ["keepSymbols", "opt.keepSymbols", true],
    ["scripts", "opt.scripts", false],
  ];

  const SIZES = [8, 9, 10, 11, 12, 14, 16, 18, 20, 24];

  // widths (px) of the results columns: File and Count; the last column (Status) takes what is left
  const cols = { w: [0, 120], statusMin: 300, touched: false };
  const COL_MIN = [120, 70, 120];

  // suggestions for the font fields (any name can be typed)
  const FONTS = [
    "Arial", "Calibri", "Cambria", "Candara", "Century Gothic", "Consolas", "Courier New", "Georgia", "Helvetica",
    "Impact", "Liberation Sans", "Liberation Serif", "Lucida Sans", "Palatino Linotype", "PT Sans", "PT Serif",
    "Segoe UI", "Tahoma", "Times New Roman", "Trebuchet MS", "Verdana", "Roboto", "Open Sans", "Noto Sans", "Noto Serif",
  ];

  // ───────────────────────── i18n ─────────────────────────
  // Bundled languages: src/locales/<code>.json listed in src/locales/index.json.
  // User languages: <app config dir>/locales/*.json (loaded through the backend).
  const FALLBACK = "en";
  const SEP = "\u001f"; // separator used by the backend in "code<SEP>arg<SEP>arg" messages
  const dicts = {};     // code -> { name, strings }
  let lang = FALLBACK;

  function lookup(code, key) {
    const d = dicts[code];
    return d && d.strings[key] !== undefined ? d.strings[key] : undefined;
  }
  function t(key, ...args) {
    let s = lookup(lang, key);
    if (s === undefined) s = lookup(FALLBACK, key);
    if (s === undefined) s = key;
    return s.replace(/\{(\d+)\}/g, (m, i) => (args[i] !== undefined ? args[i] : m));
  }
  // translate a message coming from the backend ("skip.binary", "err.empty_find\u001f3", or raw text)
  function tb(msg) {
    const [code, ...args] = String(msg).split(SEP);
    const known = lookup(lang, code) !== undefined || lookup(FALLBACK, code) !== undefined;
    return known ? t(code, ...args) : String(msg).split(SEP).join(" ");
  }
  function register(json, hintCode) {
    if (!json || typeof json !== "object") return;
    const meta = json._meta || {};
    const code = String(meta.code || hintCode || "").trim();
    if (!code) return;
    const strings = {};
    for (const [k, v] of Object.entries(json)) if (k !== "_meta" && typeof v === "string") strings[k] = v;
    dicts[code] = { name: String(meta.name || code), strings };
  }
  async function loadBundled() {
    try {
      const index = await (await fetch("locales/index.json")).json();
      for (const code of index) {
        try { register(await (await fetch(`locales/${code}.json`)).json(), code); }
        catch (e) { console.warn("locale failed:", code, e); }
      }
    } catch (e) { console.warn("locales/index.json failed", e); }
  }
  async function loadUser() {
    try {
      for (const raw of await invoke("user_locales")) {
        try { register(JSON.parse(raw)); } catch (e) { console.warn("bad user locale", e); }
      }
    } catch { /* backend without the command: ignore */ }
  }
  function populateLangs() {
    const sel = $("lang");
    const codes = Object.keys(dicts).sort((a, b) => dicts[a].name.localeCompare(dicts[b].name));
    sel.textContent = "";
    for (const code of codes) {
      const o = document.createElement("option");
      o.value = code; o.textContent = dicts[code].name;
      sel.appendChild(o);
    }
    sel.value = dicts[lang] ? lang : FALLBACK;
  }
  function detectLang(saved) {
    if (saved && dicts[saved]) return saved;
    for (const l of navigator.languages || [navigator.language || FALLBACK]) {
      const full = l.toLowerCase(), short = full.split("-")[0];
      if (dicts[full]) return full;
      if (dicts[short]) return short;
    }
    return FALLBACK;
  }
  function applyStatic() {
    document.documentElement.lang = lang;
    document.querySelectorAll("[data-i18n]").forEach((el) => { el.textContent = t(el.dataset.i18n); });
    document.querySelectorAll("[data-i18n-ph]").forEach((el) => { el.placeholder = t(el.dataset.i18nPh); });
    document.querySelectorAll("[data-i18n-title]").forEach((el) => { el.title = t(el.dataset.i18nTitle); });
  }

  // ───────────────────────── state ─────────────────────────
  const rules = [];
  let running = false;
  let lastDry = false;
  let lastFolder = "";
  let lastStats = null;
  let statusState = null;      // { key, args }
  let emptyKey = "results.empty";
  let pending = [];
  let flushScheduled = false;

  // ───────────────────────── settings ─────────────────────────
  const KEY = "font-changer-settings";
  function loadSettings() {
    try { return JSON.parse(localStorage.getItem(KEY)) || {}; } catch { return {}; }
  }
  function saveSettings() {
    const s = { folder: $("folder").value, mask: $("mask").value, maxsize: $("maxsize").value, minsize: $("minsize").value, theme: $("theme").value, lang,
      autoUpdate: $("opt-autoUpdate").checked,
      cols: cols.touched ? { w: cols.w, statusMin: cols.statusMin } : null };
    for (const [k] of OPTS) s["o_" + k] = $("opt-" + k).checked;
    try { localStorage.setItem(KEY, JSON.stringify(s)); } catch { /* ignore */ }
  }

  // ───────────────────────── switches ─────────────────────────
  for (const [key, i18n, def] of OPTS) {
    const label = document.createElement("label");
    label.className = "switch";
    label.innerHTML = '<input type="checkbox"><span class="tr"></span><span class="tx"></span>';
    label.querySelector("input").id = "opt-" + key;
    label.querySelector("input").checked = def;
    label.querySelector(".tx").dataset.i18n = i18n;
    $("switches").appendChild(label);
  }

  const st = loadSettings();
  if (st.folder) $("folder").value = st.folder;
  if (st.mask) $("mask").value = st.mask;
  if (st.maxsize !== undefined) $("maxsize").value = st.maxsize;
  if (st.minsize !== undefined) $("minsize").value = st.minsize;
  for (const [k] of OPTS) if (("o_" + k) in st) $("opt-" + k).checked = st["o_" + k];
  $("theme").value = st.theme || "system";
  if ("autoUpdate" in st) $("opt-autoUpdate").checked = st.autoUpdate;
  if (st.cols && Array.isArray(st.cols.w) && st.cols.w.length === 2) {
    cols.w = st.cols.w.map((x, i) => Math.max(COL_MIN[i], Number(x) || COL_MIN[i]));
    cols.statusMin = Math.max(COL_MIN[2], Number(st.cols.statusMin) || 300);
    cols.touched = true;
  }

  function applyTheme() {
    const v = $("theme").value;
    if (v === "system") document.documentElement.removeAttribute("data-theme");
    else document.documentElement.setAttribute("data-theme", v);
  }
  applyTheme();
  document.addEventListener("change", () => { applyTheme(); saveSettings(); });
  document.addEventListener("input", saveSettings);

  // ───────────────────────── dynamic texts ─────────────────────────
  const notify = (text, kind = "warning") => T.dialog.message(text, { title: t("dlg.title"), kind });

  function setStatus(key, ...args) { statusState = key ? { key, args } : null; renderStatus(); }
  function renderStatus() { $("status").textContent = statusState ? t(statusState.key, ...statusState.args) : ""; }
  function setEmpty(key) { emptyKey = key; renderEmpty(); }
  function renderEmpty() { $("empty").textContent = t(emptyKey); }

  function short(s, n = 90) {
    s = s.replace(/\r/g, "").replace(/\n/g, "↵");
    return s.length > n ? s.slice(0, n - 1) + "…" : s;
  }
  // "Arial → Calibri  ·  12 pt → 14 pt" (font and size are matched independently)
  function describeRule(r) {
    const parts = [];
    if (r.toFont) parts.push(`${r.fromFont || t("rule.any_font")} → ${r.toFont}`);
    if (r.toSize) parts.push(`${r.fromSize ? r.fromSize + " pt" : t("rule.any_size")} → ${r.toSize} pt`);
    if (r.scope) parts.push(t("rule.in_area", t("scope." + r.scope)));
    if (r.style) parts.push(t("rule.in_style", r.style));
    return parts.join("   ·   ");
  }
  function renderRules() {
    const box = $("rules");
    box.textContent = "";
    if (!rules.length) {
      const d = document.createElement("div");
      d.className = "none";
      d.textContent = t("rules.empty");
      box.appendChild(d);
      return;
    }
    rules.forEach((r, i) => {
      const row = document.createElement("div");
      row.className = "rule";
      const n = document.createElement("span"); n.className = "n"; n.textContent = i + 1 + ".";
      const a = document.createElement("span"); a.className = "t"; a.textContent = short(describeRule(r));
      const x = document.createElement("button"); x.textContent = "✕"; x.title = t("rule.delete");
      x.onclick = () => { rules.splice(i, 1); renderRules(); };
      row.append(n, a, x);
      box.appendChild(row);
    });
  }

  let leftoverCount = 0;

  function statusText(status, info, dry, fonts, sizes) {
    if (status === "ok") return t(dry ? "row.found" : "row.replaced", fonts, sizes);
    if (status === "leftover") return t("row.leftover", info);
    if (status === "skipped") return t("row.skipped", tb(info));
    return t("row.error", tb(info));
  }

  // After a real run: mark the files where the rules would still change something —
  // text inside embedded objects, pictures, or charts the engine could not reach.
  async function markLeftovers(req) {
    let list = [];
    try { list = await invoke("verify_run", { req }); } catch (e) { return 0; }
    if (!list || !list.length) return 0;
    const byPath = new Map(list.map((r) => [r.path, r]));
    document.querySelectorAll("#rows tr").forEach((tr) => {
      const r = byPath.get(tr.dataset.path);
      if (!r) return;
      const parts = [];
      if (r.fonts.length) parts.push(r.fonts.join(", "));
      if (r.sizes.length) parts.push(r.sizes.map((s) => s + " pt").join(", "));
      let what = parts.join(" · ");
      const why = r.hint === "embeddings" ? t("leftover.embeddings") : r.hint === "media" ? t("leftover.media") : "";
      if (why) what += " — " + why;
      tr.dataset.status = "leftover";
      tr.dataset.info = what;
      const td = tr.lastElementChild;
      td.className = "s-leftover";
      td.textContent = statusText("leftover", what, false, 0, 0);
      td.title = td.textContent;
    });
    return list.length;
  }

  function renderStats() {
    const box = $("stats");
    box.textContent = "";
    if (!lastStats) return;
    const s = lastStats;
    const items = [
      ["stat.files", s.files], ["stat.changed", s.changed], ["stat.fonts", s.fonts], ["stat.sizes", s.sizes],
      ["stat.skipped", s.skipped], ["stat.errors", s.errors],
    ];
    if (leftoverCount) items.push(["stat.leftover", leftoverCount]);
    for (const [key, val] of items) {
      const d = document.createElement("div");
      d.className = "stat" + (key === "stat.errors" && val ? " err" : "");
      const b = document.createElement("b"); b.textContent = String(val);
      const sp = document.createElement("span"); sp.textContent = t(key);
      d.append(b, sp);
      box.appendChild(d);
    }
  }

  function rerender() {
    applyStatic();
    renderRules();
    renderStatus();
    renderEmpty();
    renderStats();
    renderHelp();
    renderScan();
    document.querySelectorAll("#rows tr").forEach((tr) => {
      tr.lastChild.textContent = statusText(tr.dataset.status, tr.dataset.info, tr.dataset.dry === "1", tr.dataset.fonts, tr.dataset.sizes);
      tr.lastChild.title = tr.lastChild.textContent;
    });
  }

  // ───────────────────────── rules / dialogs ─────────────────────────
  // reads the four fields; returns null (after telling the user what is wrong) if they do not make a rule
  function readFields() {
    const r = {
      fromFont: $("fromFont").value.trim(), toFont: $("toFont").value.trim(),
      fromSize: $("fromSize").value.trim(), toSize: $("toSize").value.trim(),
      scope: $("scope").value, style: $("style").value.trim(),
    };
    if (!r.toFont && !r.toSize) { notify(t("dlg.empty_rule")); return null; }
    if (r.fromFont && !r.toFont) { notify(t("dlg.font_target")); return null; }
    if (r.fromSize && !r.toSize) { notify(t("dlg.size_target")); return null; }
    return r;
  }
  function clearFields() {
    for (const id of ["fromFont", "toFont", "fromSize", "toSize"]) $(id).value = "";
    $("scope").value = "";
    $("style").value = "";
  }

  $("addRule").onclick = () => {
    const r = readFields();
    if (!r) return;
    rules.push(r);
    clearFields();
    renderRules();
  };

  // list + the fields (if they are filled in); null = something is wrong (the user has been told)
  function collectRules() {
    const work = rules.slice();
    if ($("fromFont").value.trim() || $("toFont").value.trim() || $("fromSize").value.trim() || $("toSize").value.trim()) {
      const r = readFields();
      if (!r) return null;
      work.push(r);
    }
    return work;
  }

  // .fctpl template: rules + mask + the two options that change the result
  $("saveRules").onclick = async () => {
    try {
      const work = collectRules();
      if (!work) return;
      if (!work.length) return notify(t("dlg.nothing"));
      const p = await T.dialog.save({
        title: t("dlg.save_dialog"), defaultPath: "font-rules.fctpl",
        filters: [{ name: t("dlg.tpl_filter"), extensions: ["fctpl"] }],
      });
      if (typeof p !== "string") return;
      const mask = $("mask").value.trim();
      const template = {
        name: "",
        mask: mask && mask !== "*" ? mask : null,
        options: { keepSymbols: $("opt-keepSymbols").checked, scripts: $("opt-scripts").checked, minSize: minSize() },
        rules: work,
      };
      const written = await invoke("save_template", { path: p, template });
      notify(t("dlg.saved", written), "info");
    } catch (e) { notify(tb(e), "error"); }
  };

  $("loadRules").onclick = async () => {
    try {
      const p = await T.dialog.open({
        multiple: false, directory: false, title: t("dlg.rules_dialog"),
        filters: [{ name: t("dlg.tpl_filter"), extensions: ["fctpl", "txt", "tsv"] }],
      });
      if (typeof p !== "string") return;
      const tpl = await invoke("load_template", { path: p });
      rules.push(...tpl.rules);
      if (tpl.mask) $("mask").value = tpl.mask;
      if (tpl.options && typeof tpl.options.keepSymbols === "boolean") $("opt-keepSymbols").checked = tpl.options.keepSymbols;
      if (tpl.options && typeof tpl.options.scripts === "boolean") $("opt-scripts").checked = tpl.options.scripts;
      if (tpl.options && typeof tpl.options.minSize === "number") $("minsize").value = tpl.options.minSize;
      saveSettings();
      renderRules();
      scheduleScan();
    } catch (e) { notify(tb(e), "error"); }
  };

  // ───────────────────────── settings pop-over (gear) ─────────────────────────
  $("settingsBtn").onclick = () => $("settingsPop").classList.toggle("hidden");
  document.addEventListener("mousedown", (ev) => {
    if (!ev.target.closest(".pop-wrap")) $("settingsPop").classList.add("hidden");
  });
  document.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") $("settingsPop").classList.add("hidden");
  });

  // ───────────────────────── scan: which fonts and sizes are there? ─────────────────────────
  let scanData = { files: 0, fonts: [], sizes: [] };
  let scanState = "idle";      // idle | running | done | error
  let scanError = "";
  let scanSeq = 0;
  let scanTimer = null;

  const fmtSize = (n) => String(Number(n.toFixed(2)));

  const minSize = () => Math.max(0, parseFloat(String($("minsize").value).replace(",", ".")) || 0);

  function scanRequest() {
    return {
      folder: $("folder").value.trim(), mask: $("mask").value || "*", recursive: $("opt-recursive").checked,
      maxSizeMb: Math.max(0, Math.floor(Number($("maxsize").value) || 0)),
    };
  }

  function scheduleScan() {
    clearTimeout(scanTimer);
    if (!$("opt-autoScan").checked) return;
    scanTimer = setTimeout(runScan, 700);
  }

  async function runScan() {
    clearTimeout(scanTimer);
    const req = scanRequest();
    const seq = ++scanSeq;
    if (!req.folder) {
      invoke("cancel_scan").catch(() => {});
      scanData = { files: 0, fonts: [], sizes: [] };
      scanState = "idle";
      renderScan();
      return;
    }
    scanState = "running";
    renderScan();
    try {
      const res = await invoke("scan_fonts", { req });
      if (seq !== scanSeq) return;
      scanData = res;
      scanState = "done";
    } catch (e) {
      if (seq !== scanSeq || String(e) === "cancelled") return;
      scanData = { files: 0, fonts: [], sizes: [] };
      scanState = "error";
      scanError = tb(e);
    }
    renderScan();
  }

  // Fonts that "any font" would really change: not symbol fonts, not other scripts, not garbage names.
  const changeableFonts = () => scanData.fonts.filter((f) => !f.symbol && !f.other_script && !f.invalid);
  // Sizes of visible text (the others live only in styles or on invisible things).
  const textSizes = () => scanData.sizes.filter((x) => x.text > 0);

  // placeholders (grey text) and hints under the "Find" fields, and the summary line
  function renderScan() {
    const fonts = changeableFonts();
    const sizes = textSizes();
    const extra = scanData.sizes.length - sizes.length;
    const ready = scanState === "done";

    $("fromFont").placeholder = ready && fonts.length === 1 ? t("scan.found", fonts[0].name) : t("from.font_ph");
    $("fromSize").placeholder = ready && sizes.length === 1 ? t("scan.found", fmtSize(sizes[0].size)) : t("from.size_ph");

    let fh = "", sh = "";
    if (scanState === "running") { fh = sh = t("scan.running"); }
    else if (ready) {
      if (fonts.length === 1) fh = t("scan.hint_one", fonts[0].name);
      else if (fonts.length > 1) fh = t("scan.hint_many", fonts.length);
      if (sizes.length === 1) sh = t("scan.size_one", fmtSize(sizes[0].size));
      else if (sizes.length > 1) sh = t("scan.size_many", sizes.length);
      else if (extra > 0) sh = t("scan.size_none_text", extra);
      if (sizes.length && extra > 0) sh += " " + t("scan.size_extra", extra);
    }
    $("fromFontHint").textContent = fh;
    $("fromSizeHint").textContent = sh;

    let info = "";
    if (scanState === "running") info = t("scan.running");
    else if (scanState === "error") info = t("scan.failed", scanError);
    else if (ready) info = scanData.files ? t("scan.result", scanData.fonts.length, scanData.sizes.length, scanData.files) : t("scan.no_files");
    $("scanInfo").textContent = info;
  }

  // drop-down lists (shown when the field gets the cursor)
  function closeMenus() {
    $("fromFontMenu").classList.add("hidden");
    $("fromSizeMenu").classList.add("hidden");
  }

  // where a size was found, for the grey part of the list
  function sizeWhere(x) {
    let w;
    if (x.style > 0 && x.hidden > 0) w = t("scan.where.style_hidden");
    else if (x.hidden > 0) w = t("scan.where.hidden");
    else w = t("scan.where.style");
    return w + " · " + t("scan.files", x.files);
  }

  // groups: [{ title, items: [{ label, meta, cls, value }] }]. With a scan: what was found first, then the common ones.
  function menuGroups(isFont) {
    const ready = scanState === "done";
    const groups = [];
    if (isFont) {
      const found = ready ? scanData.fonts : [];
      const normal = found.filter((f) => !f.symbol && !f.other_script && !f.invalid);
      const special = found.filter((f) => f.symbol || f.other_script || f.invalid);
      const item = (f) => {
        const tags = [t("scan.files", f.files)];
        if (f.invalid) tags.push(t("scan.tag.invalid"));
        else if (f.symbol) tags.push(t("scan.tag.symbol"));
        else if (f.other_script) tags.push(t("scan.tag.script"));
        return { label: f.name, meta: tags.join(" · "), cls: f.symbol || f.other_script || f.invalid ? "dim" : "", value: f.name };
      };
      if (normal.length) groups.push({ title: "", items: normal.map(item) });
      if (special.length) groups.push({ title: t("scan.sec.special"), items: special.map(item) });
      const have = new Set(found.map((f) => f.name.toLowerCase()));
      const typical = FONTS.filter((f) => !have.has(f.toLowerCase()));
      groups.push({ title: found.length ? t("scan.sec.typical") : "", items: typical.map((f) => ({ label: f, meta: "", cls: "", value: f })) });
    } else {
      const found = ready ? scanData.sizes : [];
      const largestFirst = (a, b) => b.size - a.size;
      const inText = found.filter((x) => x.text > 0).sort(largestFirst);
      const other = found.filter((x) => x.text === 0).sort(largestFirst);
      const mk = (x, hidden) => ({
        label: fmtSize(x.size) + " pt",
        meta: hidden ? sizeWhere(x) : t("scan.where.text") + " · " + t("scan.files", x.text),
        cls: hidden ? "dim" : "", value: fmtSize(x.size),
      });
      if (inText.length) groups.push({ title: "", items: inText.map((x) => mk(x, false)) });
      if (other.length) groups.push({ title: t("scan.sec.hidden"), items: other.map((x) => mk(x, true)) });
      const have = new Set(found.map((x) => fmtSize(x.size)));
      const typical = SIZES.filter((x) => !have.has(fmtSize(x))).sort((a, b) => b - a);
      groups.push({ title: found.length ? t("scan.sec.typical") : "", items: typical.map((x) => ({ label: x + " pt", meta: "", cls: "", value: String(x) })) });
    }
    return groups;
  }

  function openMenu(kind) {
    const input = $("from" + kind), menu = $("from" + kind + "Menu");
    const isFont = kind === "Font";
    const q = input.value.trim().toLowerCase();

    const pick = (value) => {
      input.value = value;
      input.dispatchEvent(new Event("input", { bubbles: true }));
      closeMenus();
    };
    const add = (cls, label, meta, value) => {
      const it = document.createElement("div");
      it.className = "it " + cls;
      const nm = document.createElement("span"); nm.className = "nm"; nm.textContent = label;
      const mt = document.createElement("span"); mt.className = "meta"; mt.textContent = meta;
      it.append(nm, mt);
      // mousedown, not click: the field must not lose the focus before the choice is made
      it.addEventListener("mousedown", (ev) => { ev.preventDefault(); pick(value); });
      menu.appendChild(it);
    };

    menu.textContent = "";
    add("any", t(isFont ? "scan.any_font" : "scan.any_size"), "", "");
    let shown = 0;
    for (const g of menuGroups(isFont)) {
      const items = g.items.filter((x) => !q || (isFont ? x.value.toLowerCase().includes(q) : x.value.startsWith(q)));
      if (!items.length) continue;
      const sep = document.createElement("div"); sep.className = "sep"; menu.appendChild(sep);
      if (g.title) { const sec = document.createElement("div"); sec.className = "sec"; sec.textContent = g.title; menu.appendChild(sec); }
      for (const x of items) add(x.cls, x.label, x.meta, x.value);
      shown += items.length;
    }
    if (!shown && q) { menu.classList.add("hidden"); return; }
    menu.classList.remove("hidden");
  }

  for (const kind of ["Font", "Size"]) {
    const input = $("from" + kind);
    input.addEventListener("focus", () => openMenu(kind));
    input.addEventListener("click", () => openMenu(kind));
    input.addEventListener("input", () => openMenu(kind));
    input.addEventListener("blur", () => setTimeout(closeMenus, 120));
    input.addEventListener("keydown", (ev) => { if (ev.key === "Escape") closeMenus(); });
  }

  for (const id of ["folder", "mask", "maxsize"]) $(id).addEventListener("input", scheduleScan);
  $("opt-recursive").addEventListener("change", scheduleScan);
  $("opt-autoScan").addEventListener("change", () => {
    if ($("opt-autoScan").checked) runScan();
    else clearTimeout(scanTimer);
  });
  $("scanBtn").onclick = () => runScan();

  $("browse").onclick = async () => {
    try {
      const p = await T.dialog.open({ directory: true, multiple: false, defaultPath: $("folder").value || undefined });
      if (typeof p === "string") { $("folder").value = p; saveSettings(); scheduleScan(); }
    } catch (e) { notify(tb(e), "error"); }
  };

  $("lang").onchange = () => { lang = $("lang").value; saveSettings(); rerender(); };

  $("openLoc").onclick = async () => {
    try {
      const file = await invoke("prepare_locales_dir");
      await T.opener.revealItemInDir(file);
    } catch (e) { notify(tb(e), "error"); }
  };

  // pick up translations dropped into the user folder while the app is open
  window.addEventListener("focus", async () => {
    const before = Object.keys(dicts).join();
    await loadUser();
    if (Object.keys(dicts).join() !== before) populateLangs();
  });

  // ───────────────────────── drag & drop ─────────────────────────
  try {
    T.webview.getCurrentWebview().onDragDropEvent(async (ev) => {
      const p = ev.payload;
      if (p.type === "enter" || p.type === "over") $("drop").classList.remove("hidden");
      else if (p.type === "leave") $("drop").classList.add("hidden");
      else if (p.type === "drop") {
        $("drop").classList.add("hidden");
        if (p.paths && p.paths.length) {
          const info = await invoke("inspect_path", { path: p.paths[0] });
          $("folder").value = info.dir;
          if (info.file) $("mask").value = info.file;
          saveSettings();
          scheduleScan();
        }
      }
    });
  } catch (e) { console.warn("drag-and-drop unavailable", e); }

  // ───────────────────────── results ─────────────────────────
  function setBusy(b) {
    running = b;
    $("run").disabled = b; $("dry").disabled = b;
    $("spinner").classList.toggle("hidden", !b);
  }

  function relPath(p) {
    if (lastFolder && p.startsWith(lastFolder)) return p.slice(lastFolder.length).replace(/^[\\/]+/, "") || p;
    return p;
  }

  function flush() {
    flushScheduled = false;
    if (!pending.length) return;
    const frag = document.createDocumentFragment();
    for (const e of pending) {
      const tr = document.createElement("tr");
      tr.dataset.path = e.path; tr.dataset.status = e.status;
      tr.dataset.info = e.info; tr.dataset.dry = lastDry ? "1" : "0";
      tr.dataset.fonts = e.fonts; tr.dataset.sizes = e.sizes;
      const td1 = document.createElement("td"); td1.className = "path"; td1.title = e.path;
      const bdi = document.createElement("bdi"); bdi.textContent = relPath(e.path); td1.appendChild(bdi);
      const td2 = document.createElement("td"); td2.className = "num"; td2.textContent = e.count ? String(e.count) : "—";
      const td3 = document.createElement("td");
      td3.className = "s-" + e.status;
      td3.textContent = statusText(e.status, e.info, lastDry, e.fonts, e.sizes);
      td3.title = td3.textContent;
      tr.append(td1, td2, td3);
      frag.appendChild(tr);
    }
    pending = [];
    $("rows").appendChild(frag);
    $("empty").classList.add("hidden");
  }

  T.event.listen("file-result", (e) => {
    pending.push(e.payload);
    if (!flushScheduled) { flushScheduled = true; requestAnimationFrame(flush); }
  });

  // ───────────────────────── resizable columns of the results table ─────────────────────────
  function applyCols() {
    const avail = document.querySelector(".results").clientWidth;
    // until the user has moved a border, the File column simply takes the free space
    if (!cols.touched) cols.w[0] = Math.max(COL_MIN[0], avail - cols.w[1] - cols.statusMin);
    const total = Math.max(avail, cols.w[0] + cols.w[1] + cols.statusMin);
    $("resTable").style.width = total + "px";
    $("colFile").style.width = cols.w[0] + "px";
    $("colCount").style.width = cols.w[1] + "px";
    $("colStatus").style.width = total - cols.w[0] - cols.w[1] + "px";
  }

  function setColWidth(i, w) {
    w = Math.max(COL_MIN[i], Math.round(w));
    if (i === 2) cols.statusMin = w; else cols.w[i] = w;
    cols.touched = true;
    applyCols();
  }

  // double click on a border: make the column as wide as its longest content
  function autofitCol(i) {
    let w = 0;
    document.querySelectorAll(`#resTable tr > :nth-child(${i + 1})`).forEach((c) => { w = Math.max(w, c.scrollWidth); });
    setColWidth(i, w + 4);
    saveSettings();
  }

  document.querySelectorAll(".grip").forEach((g) => {
    const i = Number(g.dataset.col);
    g.addEventListener("pointerdown", (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      const startX = ev.clientX;
      const startW = i === 2 ? $("colStatus").getBoundingClientRect().width : cols.w[i];
      g.setPointerCapture(ev.pointerId);
      g.classList.add("act");
      document.body.classList.add("col-resizing");
      const move = (e) => setColWidth(i, startW + e.clientX - startX);
      const up = () => {
        g.removeEventListener("pointermove", move);
        g.removeEventListener("pointerup", up);
        g.removeEventListener("pointercancel", up);
        g.classList.remove("act");
        document.body.classList.remove("col-resizing");
        saveSettings();
      };
      g.addEventListener("pointermove", move);
      g.addEventListener("pointerup", up);
      g.addEventListener("pointercancel", up);
    });
    g.addEventListener("dblclick", (ev) => { ev.stopPropagation(); autofitCol(i); });
  });

  window.addEventListener("resize", applyCols);

  $("rows").addEventListener("dblclick", (ev) => {
    const tr = ev.target.closest("tr");
    if (tr && tr.dataset.path) T.opener.revealItemInDir(tr.dataset.path).catch(() => {});
  });

  // ───────────────────────── sample and report ─────────────────────────
  function fmtPt(pt) {
    return String(Math.round(pt * 100) / 100).replace(".", ",");
  }

  // take the most used font and size from a document the user picks
  $("sampleFile").onclick = async () => {
    try {
      const p = await T.dialog.open({
        multiple: false, directory: false, title: t("dlg.sample_dialog"),
        filters: [{ name: t("dlg.doc_filter"), extensions: ["docx", "docm", "dotx", "dotm", "xlsx", "xlsm", "pptx", "pptm", "odt", "ods", "odp", "rtf", "html", "htm", "css"] }],
      });
      if (typeof p !== "string") return;
      const s = await invoke("sample_file", { path: p });
      if (!s.fonts.length && !s.sizes.length) return notify(t("dlg.sample_empty"));
      clearFields();
      if (s.fonts.length) $("toFont").value = s.fonts[0][0];
      if (s.sizes.length) $("toSize").value = fmtPt(s.sizes[0][0]);
      setStatus("status.sampled", s.fonts.length ? s.fonts[0][0] : "—", s.sizes.length ? fmtPt(s.sizes[0][0]) : "—");
    } catch (e) { notify(tb(e), "error"); }
  };

  // the results table as a CSV file
  $("reportBtn").onclick = async () => {
    const rows = [...document.querySelectorAll("#rows tr")].map((tr) => ({
      path: relPath(tr.dataset.path),
      count: tr.children[1].textContent,
      status: tr.children[2].textContent,
    }));
    if (!rows.length) return notify(t("dlg.nothing"));
    try {
      const p = await T.dialog.save({
        title: t("dlg.report_dialog"), defaultPath: "font-changer-report.csv",
        filters: [{ name: t("dlg.csv_filter"), extensions: ["csv"] }],
      });
      if (typeof p !== "string") return;
      const written = await invoke("save_report", { path: p, rows });
      notify(t("dlg.report_saved", written), "info");
    } catch (e) { notify(tb(e), "error"); }
  };

  // fonts the documents use but this machine does not have
  $("missingBtn").onclick = async () => {
    const folder = $("folder").value.trim();
    if (!folder) return notify(t("dlg.no_folder"));
    try {
      const req = {
        folder, mask: $("mask").value || "*", recursive: $("opt-recursive").checked,
        maxSizeMb: Math.max(0, Math.floor(Number($("maxsize").value) || 0)),
      };
      const list = await invoke("missing_fonts", { req });
      const box = $("missingList");
      box.textContent = "";
      if (!list.length) {
        const li = document.createElement("li");
        li.textContent = t("missing.none");
        box.appendChild(li);
      } else {
        for (const f of list) {
          const li = document.createElement("li");
          li.textContent = `${f.name} — ${t("missing.files", f.files)}`;
          box.appendChild(li);
        }
      }
      showModal("missingModal");
    } catch (e) { notify(tb(e), "error"); }
  };
  $("missingClose").onclick = () => hideModal("missingModal");
  $("missingModal").addEventListener("click", (e) => { if (e.target === $("missingModal")) hideModal("missingModal"); });

  // ───────────────────────── run ─────────────────────────
  async function start(dry) {
    if (running) return;
    const work = collectRules();
    if (!work) return;
    if (!work.length) return notify(t("dlg.nothing"));
    const folder = $("folder").value.trim();
    if (!folder) return notify(t("dlg.no_folder"));

    if (!dry) {
      const ok = await T.dialog.ask(t("dlg.confirm.text", work.length, folder), {
        title: t("dlg.confirm.title"), kind: "warning", okLabel: t("dlg.yes"), cancelLabel: t("dlg.no"),
      });
      if (!ok) return;
    }

    const req = {
      folder, mask: $("mask").value || "*", rules: work,
      recursive: $("opt-recursive").checked, backup: $("opt-backup").checked,
      scripts: $("opt-scripts").checked, keepSymbols: $("opt-keepSymbols").checked,
      dry, maxSizeMb: Math.max(0, Math.floor(Number($("maxsize").value) || 0)), minSize: minSize(),
    };

    // a running scan would only compete for the disk; the folder is scanned again when the run is over
    scanSeq++;
    clearTimeout(scanTimer);
    invoke("cancel_scan").catch(() => {});
    if (scanState === "running") { scanState = "idle"; renderScan(); }

    lastDry = dry; lastFolder = folder; lastStats = null; pending = [];
    $("rows").textContent = "";
    renderStats();
    setEmpty("results.searching");
    $("empty").classList.remove("hidden");
    setStatus("status.working");
    setBusy(true);
    const t0 = performance.now();
    try {
      const s = await invoke("run_change", { req });
      flush();
      lastStats = s;
      leftoverCount = dry ? 0 : await markLeftovers(req);
      renderStats();
      const sec = ((performance.now() - t0) / 1000).toFixed(1);
      setStatus(dry ? "status.preview_done" : "status.done", sec);
      if (!s.changed && !s.errors && !s.skipped) setEmpty("results.none");
      else $("empty").classList.add("hidden");
    } catch (e) {
      setStatus("status.error");
      setEmpty("results.empty");
      notify(tb(e), "error");
    } finally {
      setBusy(false);
      scheduleScan();
    }
  }
  $("dry").onclick = () => start(true);
  $("run").onclick = () => start(false);

  // ───────────────────────── help, presets, about ─────────────────────────
  const REPO = "https://github.com/Pavel-us/FontChanger";
  let presets = [];
  let appVersion = "";
  let aboutState = null;   // { key, args }

  function openUrl(url) {
    T.opener.openUrl(url).catch((e) => console.warn("openUrl failed", e));
  }
  function showModal(id) { $(id).classList.remove("hidden"); }
  function hideModal(id) { $(id).classList.add("hidden"); }

  function selectTab(name) {
    document.querySelectorAll("#helpTabs .tab").forEach((b) => b.classList.toggle("active", b.dataset.tab === name));
    for (const n of ["about", "start", "formats", "presets"]) $("tab-" + n).classList.toggle("hidden", n !== name);
  }

  // numbered texts: start.1, start.2, ... / formats.1, ...
  function fillList(listId, prefix) {
    const ul = $(listId);
    ul.textContent = "";
    for (let i = 1; lookup(lang, `${prefix}.${i}`) !== undefined || lookup(FALLBACK, `${prefix}.${i}`) !== undefined; i++) {
      const li = document.createElement("li");
      li.textContent = t(`${prefix}.${i}`);
      ul.appendChild(li);
    }
  }

  function codeBlock(labelKey, text) {
    const frag = document.createDocumentFragment();
    const k = document.createElement("div"); k.className = "k"; k.textContent = t(labelKey);
    const c = document.createElement("code"); c.textContent = text;
    frag.append(k, c);
    return frag;
  }

  function applyPreset(p) {
    $("fromFont").value = p.fromFont; $("toFont").value = p.toFont;
    $("fromSize").value = p.fromSize; $("toSize").value = p.toSize;
    saveSettings();
    hideModal("helpModal");
    $("toFont").focus();
  }

  function renderPresets() {
    const box = $("presetsList");
    box.textContent = "";
    const any = t("preset.any");
    const size = (v) => (v ? v + " pt" : any);
    for (const p of presets) {
      const card = document.createElement("div"); card.className = "preset";
      const head = document.createElement("div"); head.className = "preset-head";
      const h = document.createElement("h4"); h.textContent = t(`preset.${p.id}.title`);
      const use = document.createElement("button"); use.className = "btn"; use.textContent = t("presets.use");
      use.onclick = () => applyPreset(p);
      head.append(h, use);
      const desc = document.createElement("p"); desc.textContent = t(`preset.${p.id}.desc`);
      card.append(head, desc);
      if (p.toFont) card.append(codeBlock("presets.font", `${p.fromFont || any}  →  ${p.toFont}`));
      if (p.toSize) card.append(codeBlock("presets.size", `${size(p.fromSize)}  →  ${size(p.toSize)}`));
      box.appendChild(card);
    }
  }

  function setAbout(key, ...args) {
    aboutState = key ? { key, args } : null;
    $("aboutStatus").textContent = aboutState ? t(key, ...args) : "";
  }

  function renderHelp() {
    fillList("startList", "start");
    fillList("formatsList", "formats");
    renderPresets();
    $("aboutVersion").textContent = appVersion ? t("about.version", appVersion) : "";
    $("aboutStatus").textContent = aboutState ? t(aboutState.key, ...aboutState.args) : "";
    renderUpdateDialog();
  }

  $("helpBtn").onclick = () => { selectTab("about"); showModal("helpModal"); };
  $("helpClose").onclick = () => hideModal("helpModal");
  $("helpModal").addEventListener("click", (e) => { if (e.target === $("helpModal")) hideModal("helpModal"); });
  document.querySelectorAll("#helpTabs .tab").forEach((b) => { b.onclick = () => selectTab(b.dataset.tab); });
  $("openRepo").onclick = () => openUrl(REPO);
  $("openDocs").onclick = () => openUrl(`${REPO}/blob/main/${lang === "ru" ? "README.ru.md" : "README.md"}`);
  $("checkUpdates").onclick = () => checkForUpdate(true);
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !updating) { hideModal("helpModal"); hideModal("updateModal"); }
  });

  // ───────────────────────── updates ─────────────────────────
  let pendingUpdate = null;
  let updating = false;
  let updMsgState = null;   // { key, args }
  const SKIP_KEY = "font-changer-skip-version";
  const getSkip = () => { try { return localStorage.getItem(SKIP_KEY) || ""; } catch { return ""; } };
  const setSkip = (v) => { try { localStorage.setItem(SKIP_KEY, v); } catch { /* ignore */ } };

  // release notes carry per-language sections: <!-- lang:en --> ... <!-- lang:ru --> ...
  function pickNotes(body) {
    const marks = [...String(body || "").matchAll(/<!--\s*lang:([a-z-]+)\s*-->/gi)];
    if (!marks.length) return String(body || "");
    const parts = {};
    marks.forEach((m, i) => {
      const start = m.index + m[0].length;
      const end = i + 1 < marks.length ? marks[i + 1].index : body.length;
      parts[m[1].toLowerCase()] = body.slice(start, end).trim();
    });
    return parts[lang] || parts[lang.split("-")[0]] || parts[FALLBACK] || Object.values(parts)[0] || "";
  }

  const stripMd = (s) => s.replace(/\*\*(.+?)\*\*/g, "$1").replace(/`([^`]+)`/g, "$1");

  // tiny, safe markdown: headings, bullets, paragraphs (everything goes through textContent)
  function renderMd(text, el) {
    el.textContent = "";
    let ul = null;
    for (const raw of text.split(/\r?\n/)) {
      const line = raw.trimEnd();
      if (!line.trim()) { ul = null; continue; }
      let m;
      if ((m = /^#{1,6}\s+(.*)$/.exec(line))) {
        ul = null;
        const h = document.createElement("h4"); h.textContent = stripMd(m[1]); el.appendChild(h);
      } else if ((m = /^\s*[-*]\s+(.*)$/.exec(line))) {
        if (!ul) { ul = document.createElement("ul"); el.appendChild(ul); }
        const li = document.createElement("li"); li.textContent = stripMd(m[1]); ul.appendChild(li);
      } else {
        ul = null;
        const p = document.createElement("p"); p.textContent = stripMd(line); el.appendChild(p);
      }
    }
  }

  function setUpdMsg(key, ...args) {
    updMsgState = key ? { key, args } : null;
    $("updateMsg").textContent = updMsgState ? t(key, ...args) : "";
  }

  function renderUpdateDialog() {
    if (!pendingUpdate) return;
    $("updateText").textContent = t("update.text", pendingUpdate.version, appVersion || "?");
    const notes = pickNotes(pendingUpdate.body).trim();
    if (notes) renderMd(notes, $("updateNotes"));
    else $("updateNotes").textContent = t("update.no_notes");
    $("updateMsg").textContent = updMsgState ? t(updMsgState.key, ...updMsgState.args) : "";
  }

  function setUpdBusy(b) {
    updating = b;
    for (const id of ["updNow", "updLater", "updSkip", "updPage"]) $(id).disabled = b;
  }

  async function showUpdateDialog() {
    let canSelf = false;
    try { canSelf = await invoke("can_self_update"); } catch { canSelf = false; }
    $("updNow").classList.toggle("hidden", !canSelf);
    $("updPage").classList.toggle("hidden", canSelf);
    $("updProgress").classList.add("hidden");
    setUpdBusy(false);
    setUpdMsg(canSelf ? null : "update.manual");
    renderUpdateDialog();
    showModal("updateModal");
  }

  async function checkForUpdate(manual) {
    try {
      if (manual) setAbout("about.checking");
      const update = await T.updater.check();
      if (!update) {
        if (manual) setAbout("about.uptodate");
        return;
      }
      pendingUpdate = update;
      $("updateBadge").classList.remove("hidden");
      if (manual) setAbout(null);
      if (!manual && getSkip() === update.version) return;
      await showUpdateDialog();
    } catch (e) {
      if (manual) setAbout("about.update_failed", String(e));
      else console.warn("update check failed:", e);
    }
  }

  $("updateBadge").onclick = () => showUpdateDialog();
  $("updLater").onclick = () => hideModal("updateModal");
  $("updSkip").onclick = () => {
    if (pendingUpdate) setSkip(pendingUpdate.version);
    $("updateBadge").classList.add("hidden");
    hideModal("updateModal");
  };
  $("updPage").onclick = () => openUrl(`${REPO}/releases/latest`);
  $("updNow").onclick = async () => {
    const u = pendingUpdate;
    if (!u || updating) return;
    setUpdBusy(true);
    $("updProgress").classList.remove("hidden");
    $("updProgress").value = 0;
    let total = 0, got = 0;
    try {
      await u.downloadAndInstall((ev) => {
        if (ev.event === "Started") {
          total = (ev.data && ev.data.contentLength) || 0;
          setUpdMsg("update.downloading", 0);
        } else if (ev.event === "Progress") {
          got += (ev.data && ev.data.chunkLength) || 0;
          const pct = total ? Math.min(100, Math.round((got * 100) / total)) : 0;
          $("updProgress").value = pct;
          setUpdMsg("update.downloading", pct);
        } else if (ev.event === "Finished") {
          $("updProgress").value = 100;
          setUpdMsg("update.installing");
        }
      });
      setUpdMsg("update.restarting");
      await T.process.relaunch();
    } catch (e) {
      setUpdBusy(false);
      $("updProgress").classList.add("hidden");
      setUpdMsg("update.error", String(e));
    }
  };

  // ───────────────────────── init ─────────────────────────
  (async () => {
    await loadBundled();
    await loadUser();
    lang = detectLang(st.lang);
    populateLangs();
    try { presets = await (await fetch("presets.json")).json(); } catch (e) { console.warn("presets.json failed", e); }
    for (const f of FONTS) { const o = document.createElement("option"); o.value = f; $("fontList").appendChild(o); }
    try { appVersion = await T.app.getVersion(); } catch { appVersion = ""; }
    rerender();
    applyCols();
    scheduleScan();
    if ($("opt-autoUpdate").checked) setTimeout(() => checkForUpdate(false), 4000);
  })();
})();
