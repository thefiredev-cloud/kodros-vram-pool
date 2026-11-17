// Polls /pool/status and /v1/models. All text goes in through textContent only,
// because reasons and model ids come from other machines.
(() => {
  const $ = (id) => document.getElementById(id);
  const el = (tag, cls, text) => {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined && text !== null) n.textContent = text;
    return n;
  };
  const gib = (mib) => (mib == null ? "?" : (mib / 1024).toFixed(1) + " GiB");
  const chip = (state, word) => {
    const c = el("span", "chip " + state);
    c.append(el("span", "dot"), el("span", "word", word || state));
    return c;
  };

  function renderTotal(st) {
    const p = st.pooled;
    const pct = p.total_mib ? Math.min(100, Math.round((p.used_mib / p.total_mib) * 100)) : 0;
    $("t-free").textContent = gib(p.free_mib) + " free";
    $("t-sub").textContent = "of " + gib(p.total_mib) + " live (" + gib(p.used_mib) + " used)";
    const m = $("t-meter");
    m.firstElementChild.style.width = pct + "%";
    m.classList.toggle("hot", pct >= 85);
    m.setAttribute("aria-label", "pooled memory " + pct + " percent used");
    $("t-up").textContent = p.devices_up + " of " + p.devices_total;
    $("t-conf").textContent = gib(p.configured_total_mib);
    const s = st.swap;
    $("t-swap").textContent = s.enabled ? s.succeeded + " ok, " + s.failed + " failed" : "off";
  }

  function renderDevice(d) {
    const u = el("section", "unit " + d.state);
    const h = el("h2");
    h.append(el("span", "name", d.label), chip(d.state));
    u.append(h);

    const mem = d.memory;
    const meter = el("div", "meter");
    const bar = el("i");
    if (mem.total_mib != null && mem.used_mib != null) {
      const pct = Math.min(100, Math.round((mem.used_mib / mem.total_mib) * 100));
      bar.style.width = pct + "%";
      if (pct >= 85) meter.classList.add("hot");
    } else {
      meter.classList.add("none");
    }
    meter.append(bar);
    u.append(meter);

    const row = (k, v) => {
      const r = el("div", "row");
      r.append(el("span", "k", k), el("span", "v", v));
      return r;
    };
    if (mem.total_mib != null && mem.used_mib != null) {
      u.append(row("memory", gib(mem.used_mib) + " / " + gib(mem.total_mib)));
    } else if (mem.total_mib != null) {
      u.append(row("memory", gib(mem.total_mib) + " (" + mem.source + ")"));
    } else {
      u.append(row("memory", "not reported"));
    }
    u.append(row("requests / s", d.requests_per_s.toFixed(2)));
    u.append(row("last tok/s", d.last_tok_s == null ? "-" : String(d.last_tok_s)));
    if (mem.load1 != null) u.append(row("load (1 min)", String(mem.load1)));

    if (d.reason) u.append(el("p", "reason", d.reason));
    const live = d.models_resident;
    if (live.length) {
      const p = el("p", "models-line");
      p.append("resident: ");
      live.forEach((m, i) => {
        if (i) p.append(", ");
        p.append(el("b", null, m));
      });
      u.append(p);
    }

    const bs = el("div", "backends");
    d.backends.forEach((b) => {
      const r = el("div", "backend");
      const left = el("span", "id", b.id + "  " + b.role);
      const right = el("span");
      right.append(chip(b.state));
      if (b.circuit !== "closed") right.append(" ", el("span", "circuit", "circuit " + b.circuit));
      r.append(left, right);
      if (b.reason && !(d.reason && d.reason.includes(b.reason))) r.append(el("span", "why", b.reason));
      bs.append(r);
    });
    u.append(bs);
    return u;
  }

  function renderModels(list) {
    const tb = document.querySelector("#models tbody");
    tb.replaceChildren();
    list.data.forEach((m) => {
      const tr = el("tr", m.available ? "" : "gone");
      tr.append(el("td", null, m.id), el("td", null, m.device), el("td", null, m.residency + (m.routable ? "" : " (decision)")),
        el("td", "num", m.ctx == null ? "-" : String(m.ctx)));
      const s = el("td");
      s.append(chip(m.available ? "up" : "down", m.available ? "live" : m.residency === "library" ? "swappable" : "down"));
      tr.append(s);
      tb.append(tr);
    });
  }

  function setConn(state, word) {
    const c = $("conn");
    c.className = "chip " + state;
    c.lastElementChild.textContent = word;
  }

  let timer = null;
  async function tick() {
    try {
      const [sr, mr] = await Promise.all([fetch("/pool/status", { cache: "no-store" }), fetch("/v1/models?all=1", { cache: "no-store" })]);
      if (!sr.ok) throw new Error("status HTTP " + sr.status);
      const st = await sr.json();
      renderTotal(st);
      const dev = $("devices");
      dev.replaceChildren(...st.devices.map(renderDevice));
      if (mr.ok) renderModels(await mr.json());
      const when = new Date(st.generated_at);
      setConn("up", "live, " + when.toLocaleTimeString());
    } catch (e) {
      setConn("down", "pool not answering: " + e.message);
    }
  }
  function loop() {
    tick();
    timer = setInterval(tick, 3000);
  }
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) { clearInterval(timer); timer = null; } else if (!timer) { loop(); }
  });
  loop();
})();
