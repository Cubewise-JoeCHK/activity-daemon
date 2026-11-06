/* activity-daemon hub UI — plain JS, no dependencies, no external fetches.
   Data sources: GET /api/machines, GET /api/calendar and the timesheet
   review APIs (/api/timesheet/*, issue #47) — all same origin. */

(() => {
  "use strict";

  const $ = (sel) => document.querySelector(sel);

  const state = {
    view: "machine",            // 'machine' | 'cal' | 'ts'
    machines: null,             // /api/machines payload
    selected: null,             // selected device name
    filter: "",                 // device list filter text
    monthAnchor: new Date(),    // mini-month month
    weekAnchor: startOfWeek(new Date()), // week grid Monday
    monthCal: null,             // month-range /api/calendar payload
    weekCal: null,              // week-range /api/calendar payload
    tsMonthAnchor: new Date(),  // timesheet day-list month
    tsDays: null,               // /api/timesheet/days payload
    tsSelectedDay: null,        // the day under review, YYYY-MM-DD
    tsDraft: null,              // /api/timesheet/draft?date= payload
    tsVocab: null,              // /api/timesheet/vocab payload
    tsQuestions: null,          // open questions payload
    tsStyle: null,              // /api/timesheet/style payload (issue #53)
    tsStyleAvailable: null,     // false once the style API 404s
    tsDirty: new Map(),         // draftId -> edited fields not yet saved
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

  function isoMonth(d) {
    return d.getFullYear() + "-" + String(d.getMonth() + 1).padStart(2, "0");
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

  async function sendJSON(url, method, body) {
    const res = await fetch(url, {
      method,
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!res.ok) throw new Error(method + " " + url + " -> HTTP " + res.status);
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
    const jobs = [];
    if (state.view === "ts") {
      // Manual refresh only — the timesheet view never auto-refreshes, so
      // in-progress row edits are never clobbered.
      jobs.push(refreshTsDays(), refreshTsDay(), refreshTsQuestions());
    } else {
      jobs.push(refreshMachines());
      if (state.view === "cal") jobs.push(refreshCalendar());
    }
    Promise.allSettled(jobs).finally(() => {
      setTimeout(() => $("#refresh-btn").classList.remove("spin"), 400);
    });
  }

  function monthRange(d) {
    return [new Date(d.getFullYear(), d.getMonth(), 1),
            new Date(d.getFullYear(), d.getMonth() + 1, 0)];
  }

  // ---------------------------------------------------------- view routing

  function viewFromHash() {
    if (location.hash.includes("calendar")) return "cal";
    if (location.hash.includes("timesheet")) return "ts";
    return "machine";
  }

  function showView(view) {
    state.view = view;
    $("#view-machine").classList.toggle("on", view === "machine");
    $("#view-cal").classList.toggle("on", view === "cal");
    $("#view-ts").classList.toggle("on", view === "ts");
    $("#rail-machine").classList.toggle("on", view === "machine");
    $("#rail-cal").classList.toggle("on", view === "cal");
    $("#rail-ts").classList.toggle("on", view === "ts");
    const hash = view === "cal" ? "calendar" : view === "ts" ? "timesheet" : "machine";
    history.replaceState(null, "", "#" + hash);
    if (view === "cal" && !state.weekCal) refreshCalendar();
    if (view === "machine" && !state.machines) refreshMachines();
    if (view === "ts") showTimesheet();
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
        for (const a of day.approved || []) blocks += approvedBlock(a);
        for (const iv of uncoveredIntervals(day)) blocks += uncoveredBlock(iv);
        for (const a of day.afk || []) blocks += afkBlock(a);
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

  // ----- processed rendering (issue #48). Approved timesheet activities =
  // coloured blocks (category colour, project label); active time not
  // covered by an approval = grey; AFK unchanged. Drafts never arrive from
  // the API, so they cannot render.

  const GRID_END = DAY_START_MIN + DAY_MINUTES; // grid bottom, 21:00

  function categoryClass(category) {
    const slug = String(category || "").toLowerCase()
      .replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");
    return slug ? "cat-" + slug : "cat-none";
  }

  function minuteOfDay(rfc3339) {
    const t = new Date(rfc3339);
    if (isNaN(t)) return null;
    return t.getHours() * 60 + t.getMinutes() + t.getSeconds() / 60;
  }

  function approvedInterval(item) {
    const a = minuteOfDay(item.start), b = minuteOfDay(item.end);
    if (a == null || b == null || b <= a) return null;
    return [Math.max(a, DAY_START_MIN), Math.min(b, GRID_END)];
  }

  function sessionInterval(item) {
    const a = minuteOfDay(item.start);
    if (a == null) return null;
    return [Math.max(a, DAY_START_MIN),
            Math.min(a + (item.duration_s || 0) / 60, GRID_END)];
  }

  function mergeIntervals(list) {
    const sorted = list.filter(Boolean).sort((x, y) => x[0] - y[0]);
    const out = [];
    for (const iv of sorted) {
      const last = out[out.length - 1];
      if (last && iv[0] <= last[1]) last[1] = Math.max(last[1], iv[1]);
      else out.push([iv[0], iv[1]]);
    }
    return out;
  }

  function subtractIntervals(base, minus) {
    const out = [];
    for (const [a, b] of base) {
      let cur = a;
      for (const [m0, m1] of minus) {
        if (m1 <= cur || m0 >= b) continue;
        if (m0 > cur) out.push([cur, Math.min(m0, b)]);
        cur = Math.max(cur, m1);
        if (cur >= b) break;
      }
      if (cur < b) out.push([cur, b]);
    }
    return out;
  }

  // Active session time minus the approved coverage, merged into blocks.
  // With no approved rows yet (empty bucket), every session is grey.
  function uncoveredIntervals(day) {
    const covered = mergeIntervals((day.approved || []).map(approvedInterval));
    const active = mergeIntervals((day.sessions || []).map(sessionInterval));
    return subtractIntervals(active, covered);
  }

  // Shared grid placement: clips to the 07:00-21:00 window, keeps a
  // readable 18 px minimum (same convention the old session blocks used).
  // Returns {top, height} in px, or null when fully outside the grid.
  function placeBlock(startMin, endMin) {
    let top = startMin - DAY_START_MIN;
    let height = (endMin - startMin) * PX_PER_MIN;
    if (top < 0) { height += top; top = 0; }
    if (top >= DAY_MINUTES || height <= 0) return null;
    height = Math.min(height, DAY_MINUTES - top);
    if (height < 18) height = 18;
    return { top, height };
  }

  function approvedBlock(item) {
    const iv = approvedInterval(item);
    if (!iv) return "";
    const p = placeBlock(iv[0], iv[1]);
    if (!p) return "";
    const tip = `${item.project || "No project"} · ${item.category || "Unclassified"}` +
      ` · ${fmtClock(item.start)}–${fmtClock(item.end)} · confidence ${Math.round((item.confidence || 0) * 100)}%`;
    return `<div class="ev ${categoryClass(item.category)}" style="top:${p.top}px;height:${p.height}px" title="${esc(tip)}">` +
      `<b>${esc(item.project || "—")}</b>` +
      `${p.height >= 30 ? `<span class="t">${fmtClock(item.start)} · ${esc(shortText(item.task))}</span>` : ""}</div>`;
  }

  function uncoveredBlock(iv) {
    const p = placeBlock(iv[0], iv[1]);
    if (!p) return "";
    return `<div class="ev uncovered" style="top:${p.top}px;height:${p.height}px" ` +
      `title="Unclassified active time · ${fmtDur((iv[1] - iv[0]) * 60)}"></div>`;
  }

  function afkBlock(item) {
    const a = minuteOfDay(item.start);
    if (a == null) return "";
    const p = placeBlock(a, a + Math.max(2, (item.duration_s || 0) / 60));
    if (!p) return "";
    return `<div class="ev afk" style="top:${p.top}px;height:${p.height}px"><b>AFK</b>` +
      `${p.height >= 30 ? `<span class="t">${fmtClock(item.start)} · ${fmtDur(item.duration_s)}</span>` : ""}</div>`;
  }

  function shortText(text) {
    return String(text || "").slice(0, 60);
  }

  // ------------------------------------------------------- timesheet view

  function tsMonthRange(d) {
    return [new Date(d.getFullYear(), d.getMonth(), 1),
            new Date(d.getFullYear(), d.getMonth() + 1, 0)];
  }

  function tsMonthLabel(d) {
    return d.toLocaleString("en-GB", { month: "long", year: "numeric" });
  }

  async function showTimesheet() {
    const monthInput = $("#ts-export-month");
    if (!monthInput.value) monthInput.value = isoMonth(new Date());
    $("#ts-export-btn").disabled = !/^\d{4}-\d{2}$/.test(monthInput.value);
    await refreshTsVocab();
    await Promise.all([refreshTsDays(), refreshTsDay(), refreshTsQuestions(), refreshTsStyle()]);
  }

  async function refreshTsVocab() {
    if (state.tsVocab) return;
    try {
      state.tsVocab = await fetchJSON("/api/timesheet/vocab");
    } catch (e) {
      state.tsVocab = { categories: [], projects: [] };
    }
  }

  async function refreshTsDays() {
    const [from, to] = tsMonthRange(state.tsMonthAnchor);
    try {
      state.tsDays = await fetchJSON(`/api/timesheet/days?from=${isoDate(from)}&to=${isoDate(to)}`);
      renderTsDays();
    } catch (e) {
      showError(e.message);
    }
  }

  function renderTsDays() {
    const el = $("#ts-days");
    if (!state.tsDays) { el.innerHTML = '<div class="empty">Loading…</div>'; return; }
    $("#ts-month-label").textContent = tsMonthLabel(state.tsMonthAnchor);
    const days = state.tsDays.days || [];
    const [first, last] = tsMonthRange(state.tsMonthAnchor);
    const inMonth = (iso) => iso >= isoDate(first) && iso <= isoDate(last);
    if (!state.tsSelectedDay || !inMonth(state.tsSelectedDay)) {
      const todayIso = isoDate(new Date());
      state.tsSelectedDay = inMonth(todayIso) ? todayIso : (days.length ? days[0].day : isoDate(first));
    }
    if (!days.length) {
      el.innerHTML = '<div class="empty">No drafted activities this month. Compile a day first (POST /api/timesheet/draft?date=…), or pick another month.</div>';
      return;
    }
    el.innerHTML = days.map((d) => `
      <button class="list-item ${d.day === state.tsSelectedDay ? "on" : ""}" data-day="${esc(d.day)}">
        <span class="li-body">
          <span class="li-name">${esc(d.day)}</span>
          <span class="day-counts">
            ${d.draft ? `<span class="dc">${d.draft} draft</span>` : ""}
            ${d.reviewed ? `<span class="dc reviewed">${d.reviewed} rev</span>` : ""}
            ${d.approved ? `<span class="dc approved">${d.approved} appr</span>` : ""}
          </span>
        </span>
      </button>`).join("");
    el.querySelectorAll(".list-item").forEach((btn) => {
      btn.addEventListener("click", () => {
        state.tsSelectedDay = btn.dataset.day;
        renderTsDays();
        refreshTsDay();
      });
    });
  }

  async function refreshTsDay() {
    if (!state.tsSelectedDay) return;
    try {
      state.tsDraft = await fetchJSON(`/api/timesheet/draft?date=${state.tsSelectedDay}`);
      state.tsDirty.clear();
      renderTsRows();
    } catch (e) {
      showError(e.message);
    }
  }

  function rowState(d) {
    return d.approved ? "approved" : d.reviewed ? "reviewed" : "draft";
  }

  function optionsWithCurrent(list, current) {
    const values = list.slice();
    if (current && !values.includes(current)) values.unshift(current);
    return values.map((v) =>
      `<option value="${esc(v)}"${v === current ? " selected" : ""}>${esc(v)}</option>`).join("");
  }

  function tsRowHtml(d) {
    const st = rowState(d);
    const sha = d.sources?.frame_sha16s?.[0];
    const frameUrl = sha
      ? `/api/timesheet/frame/${encodeURIComponent(sha)}?date=${state.tsSelectedDay}`
      : null;
    const conf = d.confidence || 0;
    const confCls = conf >= 0.8 ? "hi" : conf >= 0.6 ? "mid" : "low";
    const vocab = state.tsVocab || { projects: [], categories: [] };
    const lock = st === "approved" ? " disabled" : "";
    return `
      <div class="ts-row ${st}" data-id="${d.id}">
        <div class="ts-line1">
          <span class="ts-time">${fmtClock(d.start)}–${fmtClock(d.end)}</span>
          <span class="ts-dur">${fmtDur((new Date(d.end) - new Date(d.start)) / 1000)}</span>
          <span class="conf ${confCls}" title="analysis confidence">conf ${Math.round(conf * 100)}%</span>
          <span class="state ${st}">${st}</span>
          ${frameUrl
            ? `<a class="thumb" href="${frameUrl}" target="_blank" rel="noopener" title="evidence frame"><img src="${frameUrl}" alt="" loading="lazy" onerror="this.parentNode.style.display='none'"></a>`
            : '<span class="thumb" style="display:none"></span>'}
        </div>
        <div class="ts-grid">
          <select class="f-project" title="project"${lock}>${optionsWithCurrent(vocab.projects, d.project_guess)}</select>
          <select class="f-category" title="category"${lock}>${optionsWithCurrent(vocab.categories, d.category)}</select>
          <input class="f-task" title="task" placeholder="task" value="${esc(d.task || "")}"${lock}>
          <input class="f-comment" title="comment" placeholder="comment" value="${esc(d.comment || "")}"${lock}>
        </div>
        <div class="ts-actions">
          <button class="ts-btn ts-save" disabled${lock}>Save</button>
          ${st === "approved" ? "" : '<button class="ts-btn primary ts-approve">Approve</button>'}
        </div>
      </div>`;
  }

  function renderTsRows() {
    const el = $("#ts-rows");
    const title = $("#ts-day-title");
    const sub = $("#ts-day-sub");
    const approveAll = $("#ts-approve-all");
    if (!state.tsDraft) { el.innerHTML = '<div class="empty">Loading…</div>'; return; }
    const drafts = state.tsDraft.drafts || [];
    const counts = { draft: 0, reviewed: 0, approved: 0 };
    for (const d of drafts) counts[rowState(d)]++;
    title.textContent = state.tsSelectedDay;
    sub.textContent = `${counts.draft} draft · ${counts.reviewed} reviewed · ${counts.approved} approved` +
      (state.tsDraft.status?.generated_at ? ` · compiled ${fmtClock(state.tsDraft.status.generated_at)}` : "");
    approveAll.disabled = !drafts.length || counts.approved === drafts.length;
    if (!drafts.length) {
      el.innerHTML = '<div class="empty">No draft activities for this day. POST /api/timesheet/draft?date=' +
        esc(state.tsSelectedDay) + ' compiles one from the captured data.</div>';
      return;
    }
    el.innerHTML = drafts.map(tsRowHtml).join("");
    el.querySelectorAll(".ts-row").forEach((row) => {
      const id = Number(row.dataset.id);
      const proj = row.querySelector(".f-project");
      const cat = row.querySelector(".f-category");
      const task = row.querySelector(".f-task");
      const comment = row.querySelector(".f-comment");
      const saveBtn = row.querySelector(".ts-save");
      const approveBtn = row.querySelector(".ts-approve");
      const markDirty = () => {
        state.tsDirty.set(id, {
          project_guess: proj.value,
          category: cat.value,
          task: task.value,
          comment: comment.value,
        });
        saveBtn.disabled = false;
      };
      [proj, cat, task, comment].forEach((c) => c.addEventListener("input", markDirty));
      saveBtn.addEventListener("click", () => saveRow(id));
      if (approveBtn) approveBtn.addEventListener("click", () => approveRow(id));
    });
  }

  async function saveRow(id) {
    const edit = state.tsDirty.get(id);
    if (!edit) return;
    try {
      await sendJSON(`/api/timesheet/draft/${id}`, "PUT", edit);
      state.tsDirty.delete(id);
      await Promise.all([refreshTsDay(), refreshTsDays()]);
    } catch (e) {
      showError(e.message);
    }
  }

  async function approveRow(id) {
    try {
      await sendJSON("/api/timesheet/approve", "POST", { date: state.tsSelectedDay, draft_id: id });
      await Promise.all([refreshTsDay(), refreshTsDays()]);
    } catch (e) {
      showError(e.message);
    }
  }

  async function approveAllDay() {
    try {
      await sendJSON("/api/timesheet/approve", "POST", { date: state.tsSelectedDay });
      await Promise.all([refreshTsDay(), refreshTsDays()]);
    } catch (e) {
      showError(e.message);
    }
  }

  function tsExport() {
    const month = $("#ts-export-month").value;
    if (/^\d{4}-\d{2}$/.test(month)) window.location.href = "/api/timesheet/export?month=" + month;
  }

  async function refreshTsQuestions() {
    try {
      state.tsQuestions = await fetchJSON("/api/timesheet/questions?status=open");
    } catch (e) {
      state.tsQuestions = null;
    }
    renderTsQuestions();
  }

  function renderTsQuestions() {
    const el = $("#ts-questions");
    const qs = state.tsQuestions?.questions || [];
    if (!state.tsQuestions) { el.innerHTML = '<div class="hint">Open questions are not reachable right now.</div>'; return; }
    if (!qs.length) {
      el.innerHTML = '<div class="hint">No open questions — the analysis was confident, or nothing was captured yet.</div>';
      return;
    }
    el.innerHTML = qs.map((q) => `
      <div class="q-card" data-id="${q.id}">
        <div class="q-text">${esc(q.question)}</div>
        <div class="q-meta">${esc(q.day)} · asked ${fmtAge((Date.now() - new Date(q.created_at)) / 1000)}</div>
        ${(q.evidence || []).length ? `<div class="q-thumbs">${q.evidence.map((f) =>
          `<a href="${f.url}" target="_blank" rel="noopener" title="evidence frame"><img src="${f.url}" alt="" loading="lazy" onerror="this.parentNode.style.display='none'"></a>`).join("")}</div>` : ""}
        <div class="q-answer">
          <input placeholder="your answer" aria-label="answer for question ${q.id}">
          <button class="ts-btn primary">Answer</button>
        </div>
      </div>`).join("");
    el.querySelectorAll(".q-card").forEach((card) => {
      const id = Number(card.dataset.id);
      const input = card.querySelector("input");
      const btn = card.querySelector("button");
      const send = async () => {
        const text = input.value.trim();
        if (!text) return;
        btn.disabled = true;
        try {
          await sendJSON(`/api/timesheet/questions/${id}/answer`, "POST", { answer: text });
          await refreshTsQuestions();
        } catch (e) {
          showError(e.message);
          btn.disabled = false;
        }
      };
      btn.addEventListener("click", send);
      input.addEventListener("keydown", (e) => { if (e.key === "Enter") send(); });
    });
  }

  async function refreshTsStyle() {
    try {
      state.tsStyle = await fetchJSON("/api/timesheet/style");
      state.tsStyleAvailable = true;
    } catch (e) {
      state.tsStyleAvailable = false;
    }
    renderTsStyle();
  }

  function renderTsStyle() {
    const el = $("#ts-style");
    if (state.tsStyleAvailable === false) {
      // Graceful degradation: the style backend (issue #53) is not on this
      // hub yet — the panel re-appears as soon as the endpoint exists.
      el.innerHTML = '<div class="hint">Style settings are not on this hub yet (GET /api/timesheet/style → 404). They land with the style-prompt backend; this panel picks them up automatically after the hub updates.</div>';
      return;
    }
    const s = state.tsStyle || { style_prompt: "", style_examples: [] };
    el.innerHTML = `
      <div class="ts-style">
        <label class="lbl-note" for="ts-style-prompt">Style prompt — how generated timesheet text is written (empty = built-in default)</label>
        <textarea id="ts-style-prompt" placeholder="e.g. concise professional English, verb-first">${esc(s.style_prompt || "")}</textarea>
        <div class="lbl-note">Few-shot examples (task / comment)</div>
        <div id="ts-style-examples"></div>
        <div class="ts-actions" style="justify-content:flex-start">
          <button class="ts-btn" id="ts-style-add">Add example</button>
          <button class="ts-btn primary" id="ts-style-save">Save style</button>
        </div>
        <div class="note" id="ts-style-note">Applies to the next day compile; frame analyses pick it up after the next hub restart.</div>
      </div>`;
    const exWrap = $("#ts-style-examples");
    const addExample = (ex) => {
      const row = document.createElement("div");
      row.className = "ts-ex";
      row.innerHTML = `
        <input class="ex-task" placeholder="task example" value="${esc(ex?.task || "")}">
        <input class="ex-comment" placeholder="comment example" value="${esc(ex?.comment || "")}">
        <button class="ts-ex-del" title="remove example">remove</button>`;
      row.querySelector(".ts-ex-del").addEventListener("click", () => row.remove());
      exWrap.appendChild(row);
    };
    (s.style_examples || []).forEach(addExample);
    $("#ts-style-add").addEventListener("click", () => addExample({}));
    $("#ts-style-save").addEventListener("click", async () => {
      // The raw prompt as the owner wrote it — the backend resolves empty
      // to the built-in default and validates on its side (issue #53).
      const payload = {
        style_prompt: $("#ts-style-prompt").value,
        style_examples: [...exWrap.querySelectorAll(".ts-ex")].map((row) => ({
          task: row.querySelector(".ex-task").value,
          comment: row.querySelector(".ex-comment").value,
        })),
      };
      try {
        // sync to the disk-verified value the PUT returns
        state.tsStyle = await sendJSON("/api/timesheet/style", "PUT", payload);
        $("#ts-style-note").textContent = "Saved. Applies to the next day compile; frame analyses pick it up after the next hub restart.";
      } catch (e) {
        showError(e.message);
      }
    });
  }

  // -------------------------------------------------------------- wiring

  function wire() {
    $("#rail-machine").addEventListener("click", () => showView("machine"));
    $("#rail-cal").addEventListener("click", () => showView("cal"));
    $("#rail-ts").addEventListener("click", () => showView("ts"));
    $("#refresh-btn").addEventListener("click", refreshAll);
    $("#machine-refresh").addEventListener("click", refreshMachines);
    $("#ts-month-prev").addEventListener("click", () => {
      state.tsMonthAnchor = new Date(state.tsMonthAnchor.getFullYear(), state.tsMonthAnchor.getMonth() - 1, 1);
      refreshTsDays();
    });
    $("#ts-month-next").addEventListener("click", () => {
      state.tsMonthAnchor = new Date(state.tsMonthAnchor.getFullYear(), state.tsMonthAnchor.getMonth() + 1, 1);
      refreshTsDays();
    });
    $("#ts-refresh").addEventListener("click", () => {
      refreshTsDays();
      refreshTsDay();
      refreshTsQuestions();
    });
    $("#ts-approve-all").addEventListener("click", approveAllDay);
    $("#ts-export-month").addEventListener("change", (e) => {
      $("#ts-export-btn").disabled = !/^\d{4}-\d{2}$/.test(e.target.value);
    });
    $("#ts-export-btn").addEventListener("click", tsExport);
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
      if (e.ctrlKey && e.key === "3") { e.preventDefault(); showView("ts"); }
    });
    window.addEventListener("hashchange", () => showView(viewFromHash()));
  }

  function init() {
    wire();
    showView(viewFromHash());
    // machine/calendar auto-refresh; the timesheet view is excluded so a
    // half-edited review row is never wiped by a timer (manual refresh only)
    state.nowTimer = setInterval(() => { if (state.view !== "ts") refreshAll(); }, 30000);
    // keep the now-line honest between refreshes
    setInterval(() => { if (state.view === "cal" && state.weekCal) renderWeekGrid(); }, 60000);
  }

  init();
})();
