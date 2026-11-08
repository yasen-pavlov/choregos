/* choregos recorder script: hand-written, no dependencies, no build step. Loaded on Recorder
   pages only. Everything it does is an addition: the pages are complete without it.
   - list (/recorder): `/` focuses the first filter, j/k move a focus over the rows, Enter
     opens the focused row, t opens the timeline on it;
   - timeline: -/+ zoom, ←/→ pan (they click the toolbar links), [/] select the previous or
     next decision into the side panel, Esc closes it; hover/tap cards on ticks, markers
     and bands show their <title> text; live tail follows /recorder/live/events.
   SSE rows are read through DOMParser (an inert document) and only their attributes and
   text are used (textContent, setAttribute); server HTML enters the page only as the
   `#timeline` / `#selected` partials, swapped by htmx like the toolbar links. */
(function () {
  'use strict';

  var SVG = 'http://www.w3.org/2000/svg';
  var DAYS = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
  var MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

  // ---- small helpers (times are UTC, as the server prints them) -----------------------
  function pad(n, width) {
    var text = String(n);
    while (text.length < (width || 2)) text = '0' + text;
    return text;
  }
  function hms(d) {
    return pad(d.getUTCHours()) + ':' + pad(d.getUTCMinutes()) + ':' + pad(d.getUTCSeconds());
  }
  function clock(d) {
    return hms(d) + '.' + pad(d.getUTCMilliseconds(), 3);
  }
  function spanLine(from, to) {
    return DAYS[from.getUTCDay()] + ' ' + from.getUTCDate() + ' ' + MONTHS[from.getUTCMonth()] +
      ' · ' + hms(from) + ' → ' + hms(to);
  }
  function typing(event) {
    var target = event.target;
    return !!target && (target.isContentEditable || /^(INPUT|SELECT|TEXTAREA)$/.test(target.tagName));
  }
  function svg(name, attributes) {
    var element = document.createElementNS(SVG, name);
    Object.keys(attributes || {}).forEach(function (key) {
      element.setAttribute(key, attributes[key]);
    });
    return element;
  }
  function titled(element, text) {
    var title = svg('title');
    title.textContent = text;
    element.appendChild(title);
    return element;
  }
  function load(url, target) {
    if (window.htmx) window.htmx.ajax('GET', url, { target: target, swap: 'outerHTML' });
    else window.location.href = url.replace('/p/timeline', '/recorder/timeline');
  }

  // ---- decision list ------------------------------------------------------------------
  var focusedRow = null;
  function rows() {
    return Array.prototype.slice.call(document.querySelectorAll('#recorder-rows tr[id^="row-"]'));
  }
  function markRow(row, scroll) {
    rows().forEach(function (other) { other.classList.remove('focused'); });
    if (!row) return;
    row.classList.add('focused');
    focusedRow = row.id;
    if (scroll) row.scrollIntoView({ block: 'nearest' });
  }
  function moveRow(delta) {
    var all = rows();
    if (!all.length) return;
    var index = all.findIndex(function (row) { return row.id === focusedRow; });
    index = index < 0 ? (delta > 0 ? 0 : all.length - 1) : Math.max(0, Math.min(all.length - 1, index + delta));
    markRow(all[index], true);
  }
  function focusedLink() {
    var row = focusedRow && document.getElementById(focusedRow);
    return row && row.querySelector('a[href^="/recorder/"]');
  }
  function listKey(event) {
    var link;
    if (event.key === 'j') moveRow(1);
    else if (event.key === 'k') moveRow(-1);
    else if (event.key === 'Enter' && (link = focusedLink()) && event.target === document.body) window.location.href = link.href;
    else if (event.key === 't' && focusedRow) window.location.href = '/recorder/timeline?select=' + focusedRow.slice(4);
    else return false;
    return true;
  }

  // ---- timeline: navigation and selection ---------------------------------------------
  function timeline() { return document.getElementById('timeline'); }
  function range(element) {
    return { from: Date.parse(element.dataset.from), to: Date.parse(element.dataset.to) };
  }
  function place(t, r) {
    var x = (t - r.from) * 100 / (r.to - r.from);
    return Math.max(0, Math.min(100, x));
  }
  function positionQuery(element) {
    var query = new URLSearchParams();
    if (element.dataset.at) query.set('at', element.dataset.at);
    query.set('zoom', element.dataset.zoom);
    return query;
  }
  /* The toolbar links carry the selection; keep them (and the address bar) in step. */
  function setSelect(id) {
    var element = timeline();
    if (!element) return;
    element.querySelectorAll('[data-nav]').forEach(function (link) {
      ['href', 'hx-get', 'hx-push-url'].forEach(function (name) {
        var value = link.getAttribute(name);
        if (!value) return;
        var url = new URL(value, window.location.origin);
        if (id) url.searchParams.set('select', id); else url.searchParams.delete('select');
        link.setAttribute(name, url.pathname + url.search);
      });
    });
    if (window.htmx) window.htmx.process(element);
    var here = new URL(window.location.href);
    if (id) here.searchParams.set('select', id); else here.searchParams.delete('select');
    window.history.replaceState(window.history.state, '', here.pathname + here.search);
  }
  function markers() {
    return Array.prototype.slice.call(document.querySelectorAll('#tl-marks a.mk'));
  }
  function select(marker) {
    var element = timeline();
    if (!marker || !element) return;
    markers().forEach(function (other) { other.classList.remove('selected'); });
    marker.classList.add('selected');
    var id = marker.dataset.id;
    load('/p/timeline/select/' + encodeURIComponent(id) + '?' + positionQuery(element), '#selected');
    setSelect(id);
  }
  function step(delta) {
    var all = markers();
    if (!all.length) return;
    var index = all.findIndex(function (marker) { return marker.classList.contains('selected'); });
    index = index < 0 ? (delta > 0 ? 0 : all.length - 1) : index + delta;
    if (index >= 0 && index < all.length) select(all[index]);
  }
  function closePanel() {
    var panel = document.getElementById('selected');
    if (!panel || !panel.firstChild) return;
    panel.replaceChildren();
    markers().forEach(function (marker) { marker.classList.remove('selected'); });
    setSelect(null);
  }
  function zoom(delta) {
    var links = Array.prototype.slice.call(document.querySelectorAll('.tl-zoom a'));
    var index = links.findIndex(function (link) { return link.getAttribute('aria-current') === 'true'; });
    var next = links[index + delta];
    if (next) next.click();
  }
  function click(selector) {
    var link = document.querySelector(selector);
    if (link) link.click();
  }
  function timelineKey(event) {
    switch (event.key) {
      case '-': zoom(1); break;
      case '+': case '=': zoom(-1); break;
      case 'ArrowLeft': click('[data-pan="prev"]'); break;
      case 'ArrowRight': click('[data-pan="next"]'); break;
      case '[': step(-1); break;
      case ']': step(1); break;
      case 'Escape': closePanel(); break;
      default: return false;
    }
    return true;
  }

  // ---- hover / tap cards ----------------------------------------------------------------
  var card = null;
  var carded = null;
  function cardTarget(node) {
    return node && node.closest ? node.closest('#timeline .tk, #timeline .mk, #timeline .band, #timeline .run') : null;
  }
  /* The <title> is moved into the card while it shows, so the native tooltip does not
     appear twice; it is put back when the card hides. */
  function showCard(target) {
    var title = target.querySelector(':scope > title');
    if (!title || carded === target) return;
    hideCard();
    if (!card) {
      card = document.createElement('div');
      card.className = 'tl-card';
      card.setAttribute('role', 'tooltip');
      document.body.appendChild(card);
    }
    card.textContent = title.textContent;
    target.dataset.card = title.textContent;
    title.textContent = '';
    carded = target;
    var box = target.getBoundingClientRect();
    card.hidden = false;
    var left = Math.min(box.left, window.innerWidth - card.offsetWidth - 8);
    card.style.left = Math.max(8, left) + 'px';
    card.style.top = (box.bottom + 6) + 'px';
  }
  function hideCard() {
    if (carded) {
      var title = carded.querySelector(':scope > title');
      if (title) title.textContent = carded.dataset.card || '';
      delete carded.dataset.card;
      carded = null;
    }
    if (card) card.hidden = true;
  }

  // ---- live tail ------------------------------------------------------------------------
  /* While tailing, the window ends now. Frames from the stream are appended as ticks and
     everything slides left every second. Everything else (room, decisions and their
     verdicts, observations, the lanes' text) is the server's: a decision row or a reset
     (connect, reconnect, lag, settled verdict) re-fetches the live-edge partial, debounced
     and one at a time, and so does a slow timer, which keeps observation runs current. */
  var REFETCH_DELAY_MS = 500;
  var REFETCH_EVERY_MS = 30000;
  var GEOMETRY = { trigger: [3, 22, 3], ours: [8, 16, 3], other: [8, 16, 2], nack: [8, 16, 2] };
  var live = {
    on: false, source: null, slide: null, every: null, pending: null,
    busy: false, again: false, path: null, issued: null, held: []
  };

  /* The header counts what the lanes draw. */
  function recount() {
    var element = timeline();
    if (!element) return;
    var frames = document.getElementById('tl-n-frames');
    var decisions = document.getElementById('tl-n-decisions');
    if (frames) frames.textContent = String(element.querySelectorAll('.lane.addr .tk').length);
    if (decisions) decisions.textContent = String(element.querySelectorAll('#tl-marks .mk').length);
  }
  function refetch() {
    if (!live.on || live.pending) return;
    live.pending = window.setTimeout(function () {
      live.pending = null;
      fetchPartial();
    }, REFETCH_DELAY_MS);
  }
  /* The live edge keeps the selection (`at=now` stops a selection from centring the window). */
  function fetchPartial() {
    var element = timeline();
    if (!live.on || !element) return;
    if (live.busy) { live.again = true; return; }
    var query = new URLSearchParams({ at: 'now', zoom: element.dataset.zoom });
    var id = new URL(window.location.href).searchParams.get('select');
    if (id) query.set('select', id);
    live.busy = true;
    live.issued = element;
    live.path = '/p/timeline?' + query;
    var done = function () {
      live.busy = false;
      replayHeld();
      if (live.again) { live.again = false; refetch(); }
    };
    window.htmx.ajax('GET', live.path, { target: '#timeline', swap: 'outerHTML' }).then(done, done);
  }
  /* Frames that streamed in while the partial was on its way: the swap dropped their ticks,
     and the partial holds only those the server had when it rendered. Put back the rest
     (a tick is its lane and millisecond, as the partial's data-t spells it). */
  function replayHeld() {
    var held = live.held;
    live.held = [];
    var element = timeline();
    if (!live.on || !element) return;
    held.forEach(function (tick) {
      var iso = new Date(tick.t).toISOString();
      if (!element.querySelector('.lane.addr[data-la="' + tick.la + '"] .tk[data-t="' + iso + '"]')) addTick(tick);
    });
  }
  function addTick(tick) {
    var element = timeline();
    var track = element && element.querySelector('.lane.addr[data-la="' + tick.la + '"] .track');
    if (!track) { refetch(); return; }
    var empty = track.querySelector('.empty');
    if (empty) empty.remove();
    var shape = GEOMETRY[tick.kind] || GEOMETRY.other;
    track.appendChild(titled(svg('rect', {
      class: 'tk ' + tick.kind,
      x: place(tick.t, range(element)).toFixed(2) + '%',
      y: tick.poll ? 12 : shape[0],
      height: tick.poll ? 8 : shape[1],
      width: shape[2],
      'data-t': new Date(tick.t).toISOString()
    }), tick.title));
    recount();
  }
  /* An SSE `row` is one rendered <tr> of the live table; read its data-* attributes (and a
     cell for the title), tolerating anything missing. A decision row re-fetches. */
  function onRow(event) {
    var element = timeline();
    if (!element) return;
    var parsed = new DOMParser().parseFromString('<table><tbody>' + event.data + '</tbody></table>', 'text/html');
    var row = parsed.querySelector('tr');
    var t = row && Date.parse(row.dataset.t);
    if (!row || !t) return;
    if (row.classList.contains('decision')) { refetch(); return; }
    var from = Number(row.dataset.from);
    var to = Number(row.dataset.to);
    var bytes = (row.dataset.bytes || '').replace(/\s+/g, '').toUpperCase().match(/../g) || [];
    var ours = row.classList.contains('tx') || row.classList.contains('local');
    var poll = bytes.length === 0;
    var name = row.cells[5] ? row.cells[5].textContent.trim() : '';
    var tick = {
      la: poll && ours && from === Number(element.dataset.bridge) ? to : from,
      kind: row.dataset.ack === 'false' ? 'nack' : (ours ? 'ours' : 'other'),
      poll: poll,
      title: clock(new Date(t)) + ' · ' + from + '→' + to + ' · ' + (bytes.join(' ') || '—') + ' · ' + (name || (poll ? 'poll' : '')),
      t: t
    };
    if (tick.la === 15) return;
    if (live.busy) live.held.push(tick);
    addTick(tick);
  }
  /* Every second the window ends now: everything with a time is placed again, what slid out
     on the left is dropped, open bands stretch to now, and the axis is relabelled. */
  function slide() {
    var element = timeline();
    if (!element) return;
    var r = range(element);
    var now = Date.now();
    if (now <= r.to) return;
    r = { from: now - (r.to - r.from), to: now };
    element.dataset.from = new Date(r.from).toISOString();
    element.dataset.to = new Date(r.to).toISOString();
    element.querySelectorAll('[data-t]:not(.ax)').forEach(function (node) {
      var t = Date.parse(node.dataset.t);
      if (t < r.from) { (node.closest('.mk') || node).remove(); return; }
      node.setAttribute('x', place(t, r).toFixed(2) + '%');
    });
    element.querySelectorAll('[data-t0]').forEach(function (band) {
      var start = Math.max(Date.parse(band.dataset.t0), r.from);
      var end = band.hasAttribute('data-open') ? now : Date.parse(band.dataset.t1);
      if (end <= r.from) { band.remove(); return; }
      band.setAttribute('x', place(start, r).toFixed(2) + '%');
      band.setAttribute('width', (place(end, r) - place(start, r)).toFixed(2) + '%');
    });
    var previous = null;
    element.querySelectorAll('.ax').forEach(function (label, index) {
      var at = new Date(r.from + (r.to - r.from) * index / 6);
      var minute = at.getUTCHours() * 60 + at.getUTCMinutes();
      label.textContent = previous === minute ? ':' + pad(at.getUTCSeconds()) : hms(at);
      label.dataset.t = at.toISOString();
      previous = minute;
    });
    var line = document.getElementById('tl-span');
    if (line) line.textContent = spanLine(new Date(r.from), new Date(r.to));
    recount();
  }
  function startLive() {
    live.on = true;
    live.source = new EventSource('/recorder/live/events');
    live.source.addEventListener('row', onRow);
    live.source.addEventListener('reset', refetch);
    live.slide = window.setInterval(slide, 1000);
    live.every = window.setInterval(refetch, REFETCH_EVERY_MS);
  }
  function stopLive() {
    live.on = false;
    if (live.source) live.source.close();
    window.clearInterval(live.slide);
    window.clearInterval(live.every);
    window.clearTimeout(live.pending);
    live.source = null;
    live.slide = null;
    live.every = null;
    live.pending = null;
    live.issued = null;
    live.held = [];
  }
  /* The checkbox is re-rendered with every #timeline swap: enable it and restore its state.
     A swap to a historical window (zoom or pan to an explicit time) ends the tail. */
  function armTail() {
    var box = document.getElementById('tl-tail');
    if (!box || !('EventSource' in window) || !window.htmx) return;
    box.disabled = false;
    box.closest('label').removeAttribute('title');
    var element = timeline();
    if (live.on && element && element.dataset.at) stopLive();
    box.checked = live.on;
  }

  // ---- wiring ----------------------------------------------------------------------------
  document.addEventListener('keydown', function (event) {
    if (event.ctrlKey || event.metaKey || event.altKey || typing(event)) return;
    if (event.key === '/') {
      var filter = document.querySelector('.rec-filters select, .rec-filters input, form.filters input');
      if (filter) { event.preventDefault(); filter.focus(); }
      return;
    }
    var handled = timeline() ? timelineKey(event) : rows().length && listKey(event);
    if (handled) event.preventDefault();
  });
  document.addEventListener('change', function (event) {
    if (event.target.id !== 'tl-tail') return;
    if (event.target.checked) startLive(); else stopLive();
  });
  document.addEventListener('click', function (event) {
    var marker = event.target.closest && event.target.closest('#tl-marks a.mk');
    if (marker && event.button === 0 && !event.ctrlKey && !event.metaKey && !event.shiftKey) {
      event.preventDefault();
      select(marker);
      return;
    }
    var target = cardTarget(event.target);
    if (target) showCard(target); else hideCard();
  });
  document.addEventListener('mouseover', function (event) {
    var target = cardTarget(event.target);
    if (target) showCard(target); else hideCard();
  });
  document.addEventListener('focusin', function (event) {
    var target = cardTarget(event.target);
    if (target) showCard(target); else hideCard();
  });
  /* A live re-fetch that lands after the tail stopped, or after another swap replaced the
     window it was asked for, is dropped. */
  document.addEventListener('htmx:beforeSwap', function (event) {
    var info = event.detail.pathInfo;
    if (info && live.path && info.requestPath === live.path && (!live.on || live.issued !== timeline())) {
      event.detail.shouldSwap = false;
    }
  });
  document.addEventListener('htmx:afterSwap', function () {
    hideCard();
    armTail();
    if (focusedRow) markRow(document.getElementById(focusedRow), false);
  });
  armTail();
})();
