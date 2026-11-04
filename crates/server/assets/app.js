/* activity-daemon hub UI — plain JS, no dependencies, no external fetches.
   Data source: GET /api/machines and GET /api/calendar (same origin). */

(() => {
  "use strict";

  const $ = (sel) => document.querySelector(sel);

  const state = {
    view: "machine",            // 'machine' | 'cal'
    machines: null,             // /api/machines payload
    selected: null,             // selected device name
    filter: "",                 // device list filter text
    monthAnchor: new Date(),    // mini-month month
    weekAnchor: startOfWeek(new Date()), // week grid Monday
    monthCal: null,             // month-range /api/calendar payload
    weekCal: null,              // week-range /api/calendar payload
    nowTimer: null,
  };

  // ------------------------------------------------------------- utilities

  function esc(s) {
    return String(s ?? "").replace(/[&<>"']/g, (c) => ({
      "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
    }[c]));
  }

  function startOfWeek(d) {
    const x = new Date(d);
    x.setHours(0, 0, 0, 0);
    x.setDate(x.getDate() - ((x.getDay() + 6) % 7)); // Monday
    return x;
  }

  function addDays(d, n) {
    const x = new Date(d);
    x.setDate(x.getDate() + n);
    return x;
  }

  function isoDate(d) {
    return d.getFullYear() + "-" +
      String(d.getMonth() + 1).padStart(2, "0") + "-" +
      String(d.getDate()).padStart(2, "0");
  }

  function fmtClock(rfc3339) {
    const t = new Date(rfc3339);
    if (isNaN(t)) return "";
    return String(t.getHours()).padStart(2, "0") + ":" + String(t.getMinutes()).padStart(2, "0");
  }

  function fmtDur(seconds) {
    const s = Math.max(0, Math.round(seconds || 0));
    if (s < 60) return s + " s";
    const m = Math.round(s / 60);
    if (m < 60) return m + " min";
    const h = Math.floor(m / 60);
    return h + " h " + (m % 60) + " m";
  }

  function fmtAge(seconds) {
    const s = Math.max(0, Math.round(seconds || 0));
    if (s < 60) return s + " s ago";
    const m = Math.floor(s / 60);
    if (m < 60) return m + " min ago";
    const h = Math.floor(m / 60);
    if (h < 24) return h + " h " + (m % 60) + " m ago";
    return Math.floor(h / 24) + " d ago";
  }

  function fmtBytes(bytes) {
    const b = Number(bytes || 0);
    if (b < 1024) return b + " B";
    const kb = b / 1024;
    if (kb < 1024) return kb.toFixed(0) + " KB";
    const mb = kb / 1024;
    if (mb < 1024) return mb.toFixed(mb < 10 ? 1 : 0) + " MB";
    return (mb / 1024).toFixed(mb / 1024 < 10 ? 1 : 0) + " GB";
  }

  async function fetchJSON(url) {
    const res = await fetch(url, { cache: "no-store" });
    if (!res.ok) throw new Error(url + " -> HTTP " + res.status);
    return res.json();
  }

  function showError(msg) {
    const el = $("#error-banner");
    el.textContent = "Cannot reach the hub API" + (msg ? ": " + msg : "");
    el.hidden = false;
  }

  function clearError() {
    $("#error-banner").hidden = true;
  }

  // Presentation-only app classification (grey shades). The authoritative
  // classifier is the v0.2 config-driven rules engine on the hub.
  function classify(app) {
    const a = String(app || "").toLowerCase();
    if (/code|visual studio|vscode|cursor|zed|neovim|vim|idea|rider|rustrover|pycharm|terminal|powershell|pwsh|conhost|wsl|cmd|sublime|emacs/.test(a)) return "deep";
    if (/teams|zoom|slack|webex|meet|skype|facetime/.test(a)) return "meet";
    if (/chrome|msedge|edge|firefox|safari|brave|arc|opera/.test(a)) return "res";
    if (/outlook|mail|thunderbird|telegram|whatsapp|wechat|signal|discord/.test(a)) return "com";
    return "other";
  }

  function presenceClass(ageS) {
    if (ageS == null) return "offline";
    if (ageS < 120) return "";
    if (ageS < 3600) return "away";
    return "offline";
  }

  function presenceWord(ageS) {
    if (ageS == null) return "No events";
    if (ageS < 120) return "Active";
    if (ageS < 3600) return "Away";
    return "Offline";
  }

  // ------------------------------------------------------------- data layer

  async function refreshMachines() {
    try {
      state.machines = await fetchJSON("/api/machines");
      clearError();
      const devices = state.machines.devices || [];
      if (!devices.some((d) => d.device === state.selected)) {
        // default to the freshest device, not just the first in the list
        const freshest = devices
          .slice()
          .sort((a, b) => (a.last_seen_age_s ?? Infinity) - (b.last_seen_age_s ?? Infinity));
        state.selected = freshest.length ? freshest[0].device : null;
      }
      if (state.view === "machine") renderMachineView();
    } catch (e) {
      showError(e.message);
    }
  }

  async function refreshCalendar() {
    const [mFrom, mTo] = monthRange(state.monthAnchor);
    const wFrom = state.weekAnchor;
    const wTo = addDays(state.weekAnchor, 6);
    try {
      const [monthCal, weekCal] = await Promise.all([
        fetchJSON(`/api/calendar?from=${isoDate(mFrom)}&to=${isoDate(mTo)}`),
        fetchJSON(`/api/calendar?from=${isoDate(wFrom)}&to=${isoDate(wTo)}`),
      ]);
      state.monthCal = monthCal;
      state.weekCal = weekCal;
      clearError();
      if (state.view === "cal") renderCalendarView();
    } catch (e) {
      showError(e.message);
    }
  }

  function refreshAll() {
    $("#refresh-btn").classList.add("spin");
    const jobs = [refreshMachines()];
    if (state.view === "cal") jobs.push(refreshCalendar());
    Promise.allSettled(jobs).finally(() => {
      setTimeout(() => $("#refresh-btn").classList.remove("spin"), 400);
    });
  }

  function monthRange(d) {
    return [new Date(d.getFullYear(), d.getMonth(), 1),
            new Date(d.getFullYear(), d.getMonth() + 1, 0)];
  }

  // ---------------------------------------------------------- view routing

  function showView(view) {
    state.view = view;
    $("#view-machine").classList.toggle("on", view === "machine");
    $("#view-cal").classList.toggle("on", view === "cal");
    $("#rail-machine").classList.toggle("on", view === "machine");
    $("#rail-cal").classList.toggle("on", view === "cal");
    history.replaceState(null, "", "#" + (view === "cal" ? "calendar" : "machine"));
    if (view === "cal" && !state.weekCal) refreshCalendar();
    if (view === "machine" && !state.machines) refreshMachines();
  }

  // --------------------------------------------------------- machine view

  function deviceSubtitle(d) {
    return `${presenceWord(d.last_seen_age_s)} · last seen ${d.last_seen_age_s == null ? "never" : fmtAge(d.last_seen_age_s)} · ${d.events_today} events today`;
  }

  function renderMachineView() {
    renderHubCard();
    renderDeviceList();
    renderMachineMain();
    renderProfile();
  }

  function renderHubCard() {
    const hub = state.machines?.hub;
    const el = $("#hub-card");
    if (!hub) { el.innerHTML = '<div class="empty">Loading…</div>'; return; }
    const media = Object.values(hub.media_bytes || {}).reduce((a, b) => a + Number(b || 0), 0);
    el.innerHTML = `
      <div class="hc-name"><span class="dot-pres"></span>activity-hub</div>
      <div class="hc-sub">
        v${esc(hub.version)} · up ${fmtDur(hub.uptime_s)}<br>
        <b>${hub.events_total}</b> events · media <b>${fmtBytes(media)}</b>
      </div>`;
  }

  function renderDeviceList() {
    const el = $("#device-list");
    const devices = (state.machines?.devices || [])
      .filter((d) => d.device.toLowerCase().includes(state.filter));
    if (!devices.length) {
      el.innerHTML = '<div class="empty">' +
        (state.machines ? "No machines have reported yet." : "Loading…") + "</div>";
      return;
    }
    el.innerHTML = devices.map((d) => `
      <button class="list-item ${d.device === state.selected ? "on" : ""}" data-device="${esc(d.device)}">
        <span class="li-avatar">${esc(initials(d.device))}<span class="presence ${presenceClass(d.last_seen_age_s)}"></span></span>
        <span class="li-body">
          <span class="li-name">${esc(d.device)}</span>
          <span class="li-sub">${esc(deviceSubtitle(d))}</span>
        </span>
        <span class="li-meta">${fmtDur(d.active_today_s)}</span>
      </button>`).join("");
    el.querySelectorAll(".list-item").forEach((btn) => {
      btn.addEventListener("click", () => {
        state.selected = btn.dataset.device;
        renderMachineView();
      });
    });
  }

  function initials(name) {
    const clean = String(name || "?").replace(/[^a-zA-Z0-9]/g, " ").trim();
    const parts = clean.split(/\s+/);
    return ((parts[0]?.[0] || "?") + (parts[1]?.[0] || "")).toUpperCase() || "?";
  }

  function selectedDevice() {
    return (state.machines?.devices || []).find((d) => d.device === state.selected) || null;
  }

  function renderMachineMain() {
    const headName = $("#chat-name");
    const headSub = $("#chat-sub");
    const main = $("#machine-main");
    const d = selectedDevice();
    if (!state.machines) { main.innerHTML = '<div class="empty">Loading…</div>'; return; }
    if (!d) {
      headName.textContent = "No machines yet";
      headSub.textContent = "";
      main.innerHTML = '<div class="empty">No device has ever sent events to this hub. Start a daemon with [sync] pointing here, then refresh.</div>';
      return;
    }
    headName.textContent = d.device;
    headSub.innerHTML = `<span class="dot-pres ${presenceClass(d.last_seen_age_s)}"></span> ` +
      `${presenceWord(d.last_seen_age_s)} · last seen ${d.last_seen_age_s == null ? "never" : fmtAge(d.last_seen_age_s)} · ${d.events_total} events total`;

    const fg = d.foreground;
    const mediaRows = Object.entries(state.machines.hub?.media_bytes || {});
    const mediaTile = mediaRows.length
      ? `<div class="tile"><div class="tk">Media stored (hub)</div><div class="tv">${fmtBytes(mediaRows.reduce((a, [, b]) => a + Number(b || 0), 0))}</div><div class="td">${mediaRows.map(([k, b]) => `${esc(k)} ${fmtBytes(b)}`).join(" · ")}</div></div>`
      : "";
    const fgTile = fg
      ? `<div class="tile"><div class="tk">Foreground</div><div class="tv">${esc(fg.app || "?")}</div><div class="td">${esc(fg.title || "")} · since ${fmtClock(fg.timestamp)}</div></div>`
      : '<div class="tile"><div class="tk">Foreground</div><div class="tv">—</div><div class="td">no window events yet</div></div>';

    const feed = (d.recent || []).map(feedItem).join("");
    main.innerHTML = `
      <div class="day-divider">Latest arrivals · newest first</div>
      <div class="msg">
        <div class="msg-avatar">AD</div>
        <div class="msg-body">
          <div class="msg-meta"><b>${esc(d.device)}</b><span class="stamp">status · as of ${fmtClock(state.machines.server_time)}</span></div>
          <div class="msg-card">
            <div class="mc-title">Daily status — ${new Date().toDateString()}, ${fmtClock(state.machines.server_time)}</div>
            <div class="tile-grid">
              <div class="tile"><div class="tk">Active today</div><div class="tv">${fmtDur(d.active_today_s)}</div><div class="td">window sessions, AFK excluded</div></div>
              ${fgTile}
              <div class="tile"><div class="tk">Last seen</div><div class="tv">${d.last_seen_age_s == null ? "never" : fmtAge(d.last_seen_age_s)}</div><div class="td">${esc(d.last_seen ? fmtClock(d.last_seen) : "")}</div></div>
              <div class="tile"><div class="tk">Events today</div><div class="tv">${d.events_today}</div><div class="td">${fmtDur(d.active_today_s)} active</div></div>
              <div class="tile"><div class="tk">Events total</div><div class="tv">${d.events_total}</div><div class="td">${Object.entries(d.buckets || {}).map(([k, v]) => `${esc(k)} ${v}`).join(" · ") || "—"}</div></div>
              ${mediaTile}
            </div>
          </div>
        </div>
      </div>
      ${feed || '<div class="empty">No recent events.</div>'}`;
  }

  function feedItem(ev) {
    const data = ev.data || {};
    let tag = ev.bucket;
    let text;
    if (ev.bucket === "window-watcher") {
      tag = "window";
      text = `<b>${esc(data.app || "?")}</b> · ${esc(data.title || "")} — ${fmtDur(ev.duration_s)}`;
    } else if (ev.bucket === "afk-watcher") {
      if (data.state !== "active") return ""; // paired marker, the span event is enough
      text = `Back from AFK · away ${fmtDur(ev.duration_s)}`;
    } else if (ev.bucket === "media") {
      tag = "media";
      text = `${esc(data.kind || "file")} · ${fmtBytes(data.bytes)} · <code>${esc(data.path || "")}</code>`;
    } else {
      const pairs = Object.entries(data).slice(0, 3)
        .map(([k, v]) => `${esc(k)}=${esc(String(v))}`).join(" · ");
      text = pairs || "—";
    }
    return `
      <div class="msg">
        <div class="msg-avatar">AD</div>
        <div class="msg-body">
          <div class="msg-text"><span class="tag">${esc(tag)}</span>${text}</div>
          <div class="msg-meta"><span class="stamp">${fmtClock(ev.timestamp)}</span></div>
        </div>
      </div>`;
  }

  function renderProfile() {
    const el = $("#profile-pane");
    const d = selectedDevice();
    if (!d) { el.innerHTML = ""; return; }
    const buckets = Object.entries(d.buckets || {});
    const media = Object.entries(state.machines?.hub?.media_bytes || {});
    el.innerHTML = `
      <div class="pf-top">
        <div class="pf-avatar">${esc(initials(d.device))}</div>
        <div class="pf-name">${esc(d.device)}</div>
        <div class="pf-sub">${d.events_total} events · ${presenceWord(d.last_seen_age_s)}</div>
      </div>
      <div class="pf-rows">
        <div class="pf-row"><span class="k">Last seen</span><span class="v">${d.last_seen_age_s == null ? "never" : fmtAge(d.last_seen_age_s)}<small>${esc(d.last_seen || "")}</small></span></div>
        <div class="pf-row"><span class="k">Active today</span><span class="v">${fmtDur(d.active_today_s)}</span></div>
        <div class="pf-row"><span class="k">Events today</span><span class="v">${d.events_today}</span></div>
        <div class="pf-row"><span class="k">Foreground</span><span class="v">${esc(d.foreground?.app || "—")}</span></div>
      </div>
      <div class="pf-section">Buckets</div>
      <div class="pf-rows" style="padding-top:0">
        ${buckets.map(([k, v]) => `<div class="pf-row"><span class="k">${esc(k)}</span><span class="v">${v}</span></div>`).join("") || '<div class="empty">none</div>'}
      </div>
      ${media.length ? `
      <div class="pf-section">Hub media storage</div>
      <div class="pf-rows" style="padding-top:0">
        ${media.map(([k, b]) => `<div class="pf-row"><span class="k">${esc(k)}</span><span class="v">${fmtBytes(b)}</span></div>`).join("")}
      </div>` : ""}`;
  }

  // -------------------------------------------------------- calendar view

  const DAY_START_MIN = 7 * 60;   // grid shows 07:00 – 21:00
  const DAY_MINUTES = 14 * 60;
  const PX_PER_MIN = 1;           // 60 px per hour, like Teams

  function renderCalendarView() {
    renderMiniMonth();
    renderWeekGrid();
  }

  function renderMiniMonth() {
    const el = $("#mini-month");
    const anchor = state.monthAnchor;
    const first = new Date(anchor.getFullYear(), anchor.getMonth(), 1);
    const gridStart = startOfWeek(first);
    const byDate = new Map();
    for (const day of state.monthCal?.days || []) byDate.set(day.date, day);

    const year = anchor.getFullYear();
    const title = anchor.toLocaleString("en-GB", { month: "long", year: "numeric" });
    let html = `<div class="mini-head"><b>${esc(title)}</b></div><div class="mini-grid">`;
    for (const wd of ["M", "T", "W", "T", "F", "S", "S"]) html += `<span class="wd">${wd}</span>`;
    const todayIso = isoDate(new Date());
    for (let i = 0; i < 42; i++) {
      const d = addDays(gridStart, i);
      const iso = isoDate(d);
      const mut = d.getMonth() !== anchor.getMonth() ? " mut" : "";
      const today = iso === todayIso ? " today" : "";
      const dot = byDate.get(iso)?.active_s > 30 ? '<span class="dot"></span>' : "";
      html += `<span class="mini-day${mut}${today}">${d.getDate()}${dot}</span>`;
      if (d.getFullYear() !== year && i > 27) break; // keep <= 6 rows
    }
    el.innerHTML = html + "</div>";
    $("#cal-title").textContent = title;
  }

  function renderWeekGrid() {
    const el = $("#week-grid");
    if (!state.weekCal) { el.innerHTML = '<div class="empty">Loading…</div>'; return; }
    const days = state.weekCal.days || [];
    const byDate = new Map(days.map((d) => [d.date, d]));
    const todayIso = isoDate(new Date());
    const now = new Date();

    let header = '<div class="cal-grid"><div class="cg-head"></div>';
    let gutter = '<div class="cg-gutter">';
    let cols = "";
    const weekDays = [...Array(7)].map((_, i) => addDays(state.weekAnchor, i));

    for (let h = DAY_START_MIN / 60; h <= DAY_START_MIN / 60 + DAY_MINUTES / 60; h++) {
      gutter += `<div class="hr">${String(h % 24).padStart(2, "0")}:00</div>`;
    }
    gutter += "</div>";

    for (const d of weekDays) {
      const iso = isoDate(d);
      const we = d.getDay() === 0 || d.getDay() === 6 ? " we" : "";
      const today = iso === todayIso ? " today" : "";
      header += `<div class="cg-head${we}${today}"><div class="dh">${d.toLocaleString("en-GB", { weekday: "short" }).toUpperCase()}</div><div class="dn">${d.getDate()}</div></div>`;
      const day = byDate.get(iso);
      let blocks = '<div class="hlines"></div>';
      if (day) {
        for (const s of day.sessions || []) blocks += sessionBlock(s, false);
        for (const a of day.afk || []) blocks += sessionBlock(a, true);
      }
      if (iso === todayIso) {
        const mins = now.getHours() * 60 + now.getMinutes();
        if (mins >= DAY_START_MIN && mins <= DAY_START_MIN + DAY_MINUTES) {
          blocks += `<div class="nowline" style="top:${mins - DAY_START_MIN}px"><span class="nowtag">${fmtClock(now.toISOString())}</span></div>`;
        }
      }
      cols += `<div class="cg-col${we}">${blocks}</div>`;
    }
    el.innerHTML = header + gutter + cols + "</div>";

    $("#cal-sub").textContent =
      `Mon ${weekDays[0].getDate()} – Sun ${weekDays[6].getDate()} ${weekDays[6].toLocaleString("en-GB", { month: "short" })} · week`;
    $("#cal-device").innerHTML = `device <b>${esc(state.weekCal?.device || "—")}</b>`;
  }

  function sessionBlock(item, isAfk) {
    const start = new Date(item.start);
    if (isNaN(start)) return "";
    const startMin = start.getHours() * 60 + start.getMinutes();
    const durMin = Math.max(2, (item.duration_s || 0) / 60);
    let top = startMin - DAY_START_MIN;
    let height = durMin * PX_PER_MIN;
    if (top < 0) { height += top; top = 0; } // started before the grid window
    if (top >= DAY_MINUTES) return "";
    height = Math.min(height, DAY_MINUTES - top);
    if (height < 18) height = 18; // readable minimum, like the mockup
    if (isAfk) {
      return `<div class="ev afk" style="top:${top}px;height:${height}px"><b>AFK</b>${height >= 30 ? `<span class="t">${fmtClock(item.start)} · ${fmtDur(item.duration_s)}</span>` : ""}</div>`;
    }
    const cls = classify(item.app);
    const live = isLive(item) ? " live" : "";
    return `<div class="ev ${cls}${live}" style="top:${top}px;height:${height}px"><b>${esc(item.app || "app")}</b>${height >= 30 ? `<span class="t">${fmtClock(item.start)} · ${esc(shortTitle(item.title))}</span>` : ""}</div>`;
  }

  function shortTitle(title) {
    return String(title || "").slice(0, 60);
  }

  // A session is "live" when its span reaches now (open-ended last session
  // is flushed only on change, so the newest window event may still run).
  function isLive(session) {
    const end = new Date(new Date(session.start).getTime() + (session.duration_s || 0) * 1000);
    const last = state.weekCal?.days?.flatMap((d) => d.sessions || []).at(-1);
    return last && last.start === session.start && end.getTime() > Date.now() - 120000;
  }

  // -------------------------------------------------------------- wiring

  function wire() {
    $("#rail-machine").addEventListener("click", () => showView("machine"));
    $("#rail-cal").addEventListener("click", () => showView("cal"));
    $("#refresh-btn").addEventListener("click", refreshAll);
    $("#machine-refresh").addEventListener("click", refreshMachines);
    $("#device-filter").addEventListener("input", (e) => {
      state.filter = e.target.value.trim().toLowerCase();
      renderDeviceList();
    });
    $("#week-prev").addEventListener("click", () => {
      state.weekAnchor = addDays(state.weekAnchor, -7);
      refreshCalendar();
    });
    $("#week-next").addEventListener("click", () => {
      state.weekAnchor = addDays(state.weekAnchor, 7);
      refreshCalendar();
    });
    $("#week-today").addEventListener("click", () => {
      state.weekAnchor = startOfWeek(new Date());
      refreshCalendar();
    });
    $("#month-prev").addEventListener("click", () => {
      state.monthAnchor = new Date(state.monthAnchor.getFullYear(), state.monthAnchor.getMonth() - 1, 1);
      refreshCalendar();
    });
    $("#month-next").addEventListener("click", () => {
      state.monthAnchor = new Date(state.monthAnchor.getFullYear(), state.monthAnchor.getMonth() + 1, 1);
      refreshCalendar();
    });
    document.addEventListener("keydown", (e) => {
      if (e.key === "/" && document.activeElement?.tagName !== "INPUT") {
        e.preventDefault();
        $("#device-filter").focus();
      }
      if (e.ctrlKey && e.key === "1") { e.preventDefault(); showView("machine"); }
      if (e.ctrlKey && e.key === "2") { e.preventDefault(); showView("cal"); }
    });
    window.addEventListener("hashchange", () => {
      showView(location.hash.includes("calendar") ? "cal" : "machine");
    });
  }

  function init() {
    wire();
    showView(location.hash.includes("calendar") ? "cal" : "machine");
    refreshAll();
    state.nowTimer = setInterval(refreshAll, 30000);
    // keep the now-line honest between refreshes
    setInterval(() => { if (state.view === "cal" && state.weekCal) renderWeekGrid(); }, 60000);
  }

  init();
})();
