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
  const added = new Set();
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

  function row(song, about, button) {
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
    item.append(art, words, length);
    if (button) item.append(button);
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

  // The list under the search box: what was found, or else what's up next.
  function list() {
    if (!seen) return;
    const state = seen.state || {};
    const queue = state.queue || [];
    const me = seen.you ? seen.you.id : '';
    const queued = new Set(queue.map((song) => song.id));
    if (state.now) queued.add(state.now.id);
    const basis = JSON.stringify([results, queue, state.more, me, [...adding], [...added].filter((id) => queued.has(id))]);
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
        } else if (queued.has(song.id)) {
          button = action('done', song.title + ' is in the queue', 'check', null);
        } else {
          button = action('add', 'Add ' + song.title + ' to the queue', 'plus', () => add(song));
        }
        box.append(row(song, about, button));
      }
      if (!results.length) {
        note.hidden = false;
        note.textContent = 'Nothing in ' + host + '’s library matches that.';
      }
      return;
    }
    title.textContent = 'Up next';
    for (const song of queue) {
      const about = [song.artist];
      if (song.by) {
        const by = document.createElement('span');
        by.className = 'song-by';
        by.textContent = 'added by ' + (song.by_id === me ? 'you' : song.by);
        about.push(by);
      }
      const mine = song.by_id && song.by_id === me;
      box.append(row(song, about, mine ? action('remove', 'Take ' + song.title + ' out of the queue', 'cross', () => remove(song)) : null));
    }
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
    if (done) {
      added.add(song.id);
      toast('Added “' + song.title + '”');
    }
    drawn = '';
    list();
  }

  async function remove(song) {
    if (await ask('remove', { id: song.id })) toast('Taken out of the queue');
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
