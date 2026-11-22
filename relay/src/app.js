// The jam page: shows what the host is playing and lets a guest add to the queue.
// It talks only to the relay it came from. The relay passes questions on to the host's app.
(() => {
  'use strict';

  const root = new URL(document.body.dataset.root || './', location.href);
  const found = location.pathname.match(/\/j\/([a-z0-9]+)\/?$/);
  const jam = found ? found[1] : '';
  const api = (path) => new URL('api/jam/' + jam + path, root).toString();
  const $ = (id) => document.getElementById(id);

  const ICONS = {
    play: '<svg viewBox="0 0 24 24" aria-hidden="true"><path class="solid" d="M7 4.6v14.8a1 1 0 0 0 1.52.85l12-7.4a1 1 0 0 0 0-1.7l-12-7.4A1 1 0 0 0 7 4.6Z"/></svg>',
    pause: '<svg viewBox="0 0 24 24" aria-hidden="true"><path class="solid" d="M7.5 4h2A1.5 1.5 0 0 1 11 5.5v13A1.5 1.5 0 0 1 9.5 20h-2A1.5 1.5 0 0 1 6 18.5v-13A1.5 1.5 0 0 1 7.5 4Zm7 0h2A1.5 1.5 0 0 1 18 5.5v13a1.5 1.5 0 0 1-1.5 1.5h-2a1.5 1.5 0 0 1-1.5-1.5v-13A1.5 1.5 0 0 1 14.5 4Z"/></svg>',
    grip: '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5 9h14M5 15h14"/></svg>',
    plus: '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 5v14M5 12h14"/></svg>',
    check: '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m5 12.5 4.5 4.5L19 7.5"/></svg>',
    cross: '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M18 6 6 18M6 6l12 12"/></svg>',
  };

  // What's kept between visits: the name last used, and this jam's pass.
  const store = {
    get(key) {
      try { return localStorage.getItem(key) || ''; } catch { return ''; }
    },
    set(key, value) {
      try { value ? localStorage.setItem(key, value) : localStorage.removeItem(key); } catch { /* private browsing */ }
    },
  };
  const PASS = 'nexus-jam:' + jam;
  const NAME = 'nexus-jam-name';

  let token = store.get(PASS);
  let host = '';
  let seen = null;        // the jam as last heard: { v, state, guests, you, away, age }
  let heardAt = 0;        // when that arrived, by this device's clock
  let version = 0;
  let rejoined = false;   // one quiet attempt to rejoin with the remembered name
  let results = null;     // songs found for what's typed; null when nothing is typed
  let searching = 0;      // counts searches, so a late answer isn't shown
  let drawn = '';         // what the list was last drawn from
  const adding = new Set();
  let toastTimer = 0;

  const clock = (seconds) => {
    const whole = Math.max(0, Math.floor(seconds));
    return Math.floor(whole / 60) + ':' + String(whole % 60).padStart(2, '0');
  };

  function toast(text, bad) {
    const box = $('toast');
    box.textContent = text;
    box.classList.toggle('bad', !!bad);
    box.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { box.hidden = true; }, bad ? 4500 : 2500);
  }

  // The screen for when there's no jam on show: before joining, or after it's over.
  function gate(title, text, form) {
    $('jam').hidden = true;
    $('gate').hidden = false;
    $('gate-title').textContent = title;
    $('gate-text').textContent = text;
    $('join').hidden = !form;
    if (form) {
      const name = $('name');
      if (!name.value) name.value = store.get(NAME);
      $('join-button').disabled = false;
    }
  }

  async function call(path, options) {
    const headers = { ...(options && options.headers) };
    if (token) headers.Authorization = 'Bearer ' + token;
    const response = await fetch(api(path), { cache: 'no-store', ...options, headers });
    let body = null;
    try { body = await response.json(); } catch { /* not JSON */ }
    return { status: response.status, body: body || {} };
  }

  const post = (path, body) => call(path, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });

  async function join(name) {
    const { status, body } = await post('/join', { name });
    if (status === 404) return over();
    if (status !== 200) throw new Error(body.error || 'Joining didn’t work. Try again.');
    token = body.token;
    store.set(PASS, token);
    store.set(NAME, name);
    return true;
  }

  function over() {
    token = '';
    store.set(PASS, '');
    gate('This jam has ended', 'Ask whoever was hosting for a new link.', false);
    return false;
  }

  // Asks the host's app to do something. Resolves to its answer, or null after saying why not.
  async function ask(action, data) {
    try {
      const { status, body } = await post('/ask', { action, data: data || {} });
      if (status === 200 && body.ok) return body.data || {};
      if (status === 404) { over(); return null; }
      toast(body.error || 'That didn’t work. Try again.', true);
    } catch {
      toast('No connection. Try again.', true);
    }
    return null;
  }

  // Keeps asking the relay what's changed; each question is held open until something has.
  async function watch() {
    let wait = 1000;
    for (;;) {
      let answer;
      try {
        answer = await call('/state?v=' + version);
      } catch {
        notice('Reconnecting…');
        await new Promise((done) => setTimeout(done, wait));
        wait = Math.min(wait * 2, 10000);
        continue;
      }
      wait = 1000;
      const { status, body } = answer;
      if (status === 200) {
        seen = body;
        version = body.v;
        heardAt = performance.now();
        rejoined = false;
        draw();
      } else if (status === 404) {
        return over();
      } else if (status === 403) {
        token = '';
        store.set(PASS, '');
        return gate('You’ve been removed from this jam', 'The host took you out of it.', false);
      } else if (status === 401) {
        // The relay doesn't know this pass (it restarted, say). Rejoin under the same name, once.
        token = '';
        store.set(PASS, '');
        const name = store.get(NAME);
        if (name && !rejoined) {
          rejoined = true;
          let back = null;
          try { back = await join(name); } catch { /* fall through to the form */ }
          if (back === true) continue;
          if (back === false) return;
        }
        return gate(host ? 'Join ' + host + '’s jam' : 'Join the jam', 'Add songs to the queue from here. The music plays where the host is.', true);
      } else {
        notice('Reconnecting…');
        await new Promise((done) => setTimeout(done, 2000));
      }
    }
  }

  function notice(text) {
    const box = $('notice');
    box.textContent = text;
    box.hidden = !text;
  }

  // Where the song has got to: where the host's app said it was, plus the time since.
  function position() {
    const state = seen && seen.state;
    if (!state || !state.now) return 0;
    const since = state.playing && !seen.away ? (seen.age || 0) + (performance.now() - heardAt) / 1000 : 0;
    return Math.min((state.position || 0) + since, state.now.duration || 0);
  }

  function tick() {
    const state = seen && seen.state;
    if (!state || !state.now || $('jam').hidden) return;
    const at = position();
    const all = state.now.duration || 0;
    $('bar-fill').style.width = (all ? Math.min(100, (at / all) * 100) : 0) + '%';
    $('time-at').textContent = clock(at);
    $('time-all').textContent = clock(all);
  }

  function cover(id, size) {
    return id ? api('/cover/' + size + '/' + encodeURIComponent(id)) : '';
  }

  function draw() {
    if (!seen) return;
    const state = seen.state || {};
    const now = state.now;
    $('gate').hidden = true;
    $('jam').hidden = false;
    host = seen.host || host;
    $('host').textContent = host + '’s jam';
    document.title = host + '’s jam · Nexus Music';

    const guests = seen.guests || [];
    $('people-count').textContent = guests.length + 1 + ' here';
    const people = $('people-list');
    people.replaceChildren();
    const person = (name, kind) => {
      const item = document.createElement('li');
      item.textContent = name;
      if (kind) item.className = kind;
      people.append(item);
    };
    person(host + ' (host)', 'host');
    for (const guest of guests) {
      const you = seen.you && guest.id === seen.you.id;
      person(you ? guest.name + ' (you)' : guest.name, you ? 'you' : '');
    }

    notice(seen.away ? host + '’s app isn’t connected right now. It should be back shortly.' : '');

    const art = $('now-art');
    const picture = now ? cover(now.cover, 'l') : '';
    if (art.dataset.src !== picture) {
      art.dataset.src = picture;
      art.hidden = true;
      if (picture) art.src = picture; else art.removeAttribute('src');
    }
    $('now-title').textContent = now ? now.title : 'Nothing is playing yet';
    $('now-artist').textContent = now ? now.artist : 'Add a song to get it started.';
    $('progress').hidden = !now;
    const control = !!state.control && !!now;
    $('controls').hidden = !control;
    const note = $('controls-note');
    note.hidden = control || !now;
    note.textContent = host + ' is choosing what plays. You can still add songs.';
    const toggle = $('toggle');
    const wanted = state.playing ? 'pause' : 'play';
    if (toggle.dataset.icon !== wanted) {
      toggle.dataset.icon = wanted;
      toggle.innerHTML = ICONS[wanted];
      toggle.setAttribute('aria-label', state.playing ? 'Pause' : 'Play');
    }
    tick();
    list();
  }

  function row(song, about, buttons) {
    const item = document.createElement('li');
    const picture = cover(song.cover, 's');
    let art;
    if (picture) {
      art = document.createElement('img');
      art.alt = '';
      art.loading = 'lazy';
      art.width = 48;
      art.height = 48;
      art.src = picture;
      art.addEventListener('error', () => { art.removeAttribute('src'); art.className = 'blank'; }, { once: true });
    } else {
      art = document.createElement('div');
      art.className = 'blank';
    }
    const words = document.createElement('div');
    words.className = 'song-words';
    const title = document.createElement('p');
    title.className = 'song-title';
    title.textContent = song.title;
    const line = document.createElement('p');
    line.className = 'song-about';
    const first = document.createElement('span');
    first.className = 'song-artist';
    first.textContent = about[0];
    line.append(first, ...about.slice(1));
    words.append(title, line);
    const length = document.createElement('span');
    length.className = 'song-length';
    length.textContent = clock(song.duration || 0);
    item.append(art, words, length, ...buttons);
    return item;
  }

  function action(kind, label, icon, run) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'act ' + kind;
    button.setAttribute('aria-label', label);
    button.innerHTML = ICONS[icon];
    if (run) button.addEventListener('click', run); else button.disabled = true;
    return button;
  }

  // The heading between the songs queued by hand and the rest of the list.
  function heading(text) {
    const item = document.createElement('li');
    item.className = 'heading';
    item.textContent = text;
    return item;
  }

  // The list under the search box: what was found, or else what's up next.
  function list() {
    if (!seen || drag) return;
    const state = seen.state || {};
    const queue = state.queue || [];
    const me = seen.you ? seen.you.id : '';
    const control = !!state.control;
    // Songs waiting in "Next in queue", and the one playing: adding those again does nothing.
    const waiting = new Set(queue.filter((song) => song.queued).map((song) => song.id));
    if (state.now) waiting.add(state.now.id);
    const basis = JSON.stringify([results, queue, state.more, state.context, control, me, [...adding]]);
    if (basis === drawn) return;
    drawn = basis;
    const box = $('list');
    const title = $('list-title');
    const note = $('list-note');
    box.replaceChildren();
    note.hidden = true;
    if (results) {
      title.textContent = 'Songs';
      for (const song of results) {
        const about = [song.album ? song.artist + ' · ' + song.album : song.artist];
        let button;
        if (adding.has(song.id)) {
          button = action('add busy', 'Adding ' + song.title, 'plus', null);
        } else if (waiting.has(song.id)) {
          button = action('done', song.title + ' is in the queue', 'check', null);
        } else {
          button = action('add', 'Add ' + song.title + ' to the queue', 'plus', () => add(song));
        }
        box.append(row(song, about, [button]));
      }
      if (!results.length) {
        note.hidden = false;
        note.textContent = 'Nothing in ' + host + '’s library matches that.';
      }
      return;
    }
    const ahead = queue.findIndex((song) => !song.queued);
    const queued = ahead < 0 ? queue.length : ahead;
    const rest = state.context ? 'Next from ' + state.context : 'Next up';
    title.textContent = queued ? 'Next in queue' : rest;
    queue.forEach((song, at) => {
      if (queued && at === queued) box.append(heading(rest));
      const about = [song.artist];
      if (song.by) {
        const by = document.createElement('span');
        by.className = 'song-by';
        by.textContent = 'added by ' + (song.by_id === me ? 'you' : song.by);
        about.push(by);
      }
      const buttons = [];
      const mine = song.queued && song.by_id && song.by_id === me;
      if (control || mine) buttons.push(action('remove', 'Take ' + song.title + ' out of the queue', 'cross', () => remove(song, at)));
      if (control) {
        const grip = action('grip', 'Move ' + song.title, 'grip', null);
        grip.disabled = false;
        grip.addEventListener('pointerdown', (event) => grab(event, at));
        buttons.push(grip);
      }
      const item = row(song, about, buttons);
      item.dataset.at = at;
      box.append(item);
    });
    if (state.more) {
      note.hidden = false;
      note.textContent = 'And ' + state.more + ' more after these.';
    } else if (!queue.length) {
      note.hidden = false;
      note.textContent = 'Nothing is queued. Find a song above to add one.';
    }
  }

  async function add(song) {
    adding.add(song.id);
    list();
    const done = await ask('add', { id: song.id });
    adding.delete(song.id);
    if (done) toast('Added “' + song.title + '”');
    drawn = '';
    list();
  }

  async function remove(song, at) {
    // Shown as done straight away; the host's word on it follows.
    seen.state.queue.splice(at, 1);
    list();
    if (!(await ask('remove', { at, id: song.id }))) refresh();
  }

  // Asks for the jam as it stands, after something shown as done turned out not to be.
  async function refresh() {
    try {
      const { status, body } = await call('/state?v=0');
      if (status === 200) {
        seen = body;
        version = body.v;
        heardAt = performance.now();
        draw();
      }
    } catch { /* the next change will put it right */ }
  }

  // ---- dragging a song to a new place in the queue ----

  let drag = null;   // { at, item, line, target, queued }

  function grab(event, at) {
    const item = event.currentTarget.closest('li');
    if (!item || drag) return;
    event.preventDefault();
    const line = document.createElement('li');
    line.className = 'drop';
    drag = { at, item, line, target: at, queued: !!seen.state.queue[at].queued, pointer: event.pointerId };
    item.classList.add('dragging');
    event.currentTarget.setPointerCapture(event.pointerId);
    event.currentTarget.addEventListener('pointermove', carry);
    event.currentTarget.addEventListener('pointerup', drop);
    event.currentTarget.addEventListener('pointercancel', drop);
  }

  // Works out where the song would land: before which song, and in which part of the queue.
  function carry(event) {
    if (!drag) return;
    const box = $('list');
    const rows = [...box.children].filter((child) => child !== drag.line);
    // Near the top or bottom of the screen, the page scrolls to follow.
    if (event.clientY < 80) scrollBy(0, -12); else if (event.clientY > innerHeight - 80) scrollBy(0, 12);
    let before = null;
    for (const child of rows) {
      const area = child.getBoundingClientRect();
      if (event.clientY < area.top + area.height / 2) { before = child; break; }
    }
    box.insertBefore(drag.line, before);
    // Count the songs above the line, and see which side of the heading it's on.
    let target = 0;
    let queued = true;
    for (const child of box.children) {
      if (child === drag.line) break;
      if (child.classList.contains('heading')) queued = false; else target += 1;
    }
    const queue = seen.state.queue;
    const split = queue.findIndex((song) => !song.queued);
    drag.target = target;
    drag.queued = queued && (split < 0 ? queue.length : split) > 0;
  }

  function drop(event) {
    if (!drag) return;
    const { at, item, line, queued } = drag;
    let { target } = drag;
    event.currentTarget.removeEventListener('pointermove', carry);
    event.currentTarget.removeEventListener('pointerup', drop);
    event.currentTarget.removeEventListener('pointercancel', drop);
    item.classList.remove('dragging');
    line.remove();
    drag = null;
    const queue = seen.state.queue;
    // `target` counted the song itself if it was above the line.
    const to = target > at ? target - 1 : target;
    if (event.type !== 'pointerup' || (to === at && !!queue[at].queued === queued)) return list();
    const song = queue.splice(at, 1)[0];
    if (queued) song.queued = true; else delete song.queued;
    queue.splice(to, 0, song);
    drawn = '';
    list();
    ask('move', { from: at, to, queued, id: song.id }).then((done) => { if (!done) refresh(); });
  }

  let typing = 0;
  function typed() {
    const text = $('query').value.trim();
    $('clear').hidden = !$('query').value;
    clearTimeout(typing);
    const mine = ++searching;
    if (!text) {
      results = null;
      return list();
    }
    typing = setTimeout(async () => {
      const got = await ask('search', { q: text });
      if (mine !== searching || !got) return;
      results = Array.isArray(got.songs) ? got.songs : [];
      list();
    }, 280);
  }

  async function begin() {
    if (!jam) {
      return gate('Nexus Music jams', 'To join a jam, open the link its host sent you.', false);
    }
    let about;
    try {
      about = await fetch(api(''), { cache: 'no-store' });
    } catch {
      return gate('Can’t reach the jam', 'Check your connection, then reload this page.', false);
    }
    if (about.status === 404) return over();
    try { host = (await about.json()).host || ''; } catch { /* shown without a name */ }
    if (token) return watch();
    gate(host ? 'Join ' + host + '’s jam' : 'Join the jam', 'Add songs to the queue from here. The music plays where the host is.', true);
  }

  $('join').addEventListener('submit', async (event) => {
    event.preventDefault();
    const name = $('name').value.trim();
    if (!name) return;
    $('join-button').disabled = true;
    try {
      if (await join(name)) watch();
    } catch (problem) {
      $('join-button').disabled = false;
      toast(problem.message || 'Joining didn’t work. Try again.', true);
    }
  });

  $('people').addEventListener('click', () => {
    const open = $('people-list').hidden;
    $('people-list').hidden = !open;
    $('people').setAttribute('aria-expanded', String(open));
  });

  $('toggle').addEventListener('click', async () => {
    if (!seen || !seen.state) return;
    $('toggle').disabled = true;
    await ask(seen.state.playing ? 'pause' : 'play');
    $('toggle').disabled = false;
  });

  $('skip').addEventListener('click', async () => {
    $('skip').disabled = true;
    await ask('skip');
    $('skip').disabled = false;
  });

  $('previous').addEventListener('click', async () => {
    $('previous').disabled = true;
    await ask('previous');
    $('previous').disabled = false;
  });

  $('query').addEventListener('input', typed);
  $('clear').addEventListener('click', () => {
    $('query').value = '';
    typed();
    $('query').focus();
  });

  $('now-art').addEventListener('load', () => { $('now-art').hidden = false; });
  $('now-art').addEventListener('error', () => { $('now-art').hidden = true; });

  setInterval(tick, 500);
  begin();
})();
