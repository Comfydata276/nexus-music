# The Nexus server

One small service of our own, in one container, beside the music server. The design is
`docs/backend-design.md` on its own branch. It answers `hello` (S1), keeps its data and a
store kept apart (S3), signs people in with an email and a password (S8), and hands each of a
person's devices a music login made for it from the one an admin typed in (S9). The admin's
page's other calls (S10b) are not in yet.

## Building and testing

Built apart from the app, as the relay is:

    cargo test --manifest-path server/Cargo.toml
    docker build -f server/Dockerfile -t nexus-music-server .

`tests/e6.rs` is the design's experiment E6, over real sockets on loopback. Every bound it
holds the backend to is a number in `src/bounds.rs`. `tests/x2.rs` is X2 (the data and the
store kept apart, cut, put back and lost); `tests/signin.rs` signs a person in end to end
against the program itself; `tests/program.rs` is the program's settings, log and health.

## Settings

| Setting | |
|---|---|
| `NEXUS_MUSIC_SERVER` | The music server as the container reaches it, for example `http://navidrome:4533` (plain HTTP is fine: it is inside the container's network). **Must be given, and only here**: never set from the page. **The only address the backend ever connects to**: a login typed in is checked here and nowhere else; a login checked at another value of this is "cannot be used now" until it is typed again. |
| `NEXUS_DATA_BUDGET` | How large the data may grow before new events wait: bytes, or a whole number of MB, GB or TB (`4GB`), at least 1 MB. 4 GB unless given. |
| `NEXUS_LIDARR` | Lidarr as this container reaches it, for requests through the backend, for example `http://lidarr:8686` (plain HTTP is fine inside the container's network; no name, password, query or `#`). Not given: requests through the backend are not set up. It is set here only, never from the admin's page; the key is typed on the page and sealed to this address. |
| `SSL_CERT_FILE` | Optional, and only if `NEXUS_MUSIC_SERVER` is HTTPS: a file of the certificates the backend trusts for it, in place of the container's own. |
| `NEXUS_PROXY` | Who the proxy is: the one IP address it connects to this container from, for example `172.18.0.2`, or the name of the proxy's container on a network the two share, for example `Nginx-Proxy-Manager-Official` (see below). **Must be given.** Nothing else is answered. |
| `NEXUS_DATA` | The data folder: one SQLite file, `nexus.db`, and its nightly copies in `copies/`. `/data` unless given. On a disk of the machine's own, not a network folder. |
| `NEXUS_STATE` | The folder of the store kept apart: the backend's id (`id`), its restore number, its keys, and what must never go backwards when the data does (`kept`). `/state` unless given. Not the data folder, inside it or around it, through a link or a mount either. **Never copied or put back with the data.** |
| `NEXUS_LISTEN` | `0.0.0.0:8080` unless given. Not port 0. The health check asks on this machine's loopback, so listen on every address or on loopback. |
| `NEXUS_PUBLIC_ORIGIN` | The backend's own HTTPS name, which nothing else shares (below), as a browser has it: `https://` and the name, and `:port` only if it is not 443, nothing after it, for example `https://nexus.example.org`. The admin's page's sign-in and set-up are taken only from it. Unset, they are refused, and the log says so once at the start. |

A setting that is wrong or missing is said by name as it starts, and it stops.

## The owner's commands

Each is run in the container **while the backend is stopped** (it refuses while the backend
serves: one program has the folders at a time), with the same settings, for example
`docker run --rm --env-file nexus.env -v …:/data -v …:/state nexus-music-server set-up you@example.org`.

| Command | |
|---|---|
| `nexus-music-server health` | The container's own check: `hello` answers with the id kept, and the store kept apart and the data each open and are this backend's. |
| `nexus-music-server set-up <email> [<name>]` | Sets the backend up and makes its first admin, **without printing anything to the log**: a one-time code is said on this command's own terminal, once. Sign in with it from the app, choosing a password there. Refused once set up. |
| `nexus-music-server recover <email>` | A one-time code for the account with that email, the owner's own included, with no sign-in: every session of it ends, its password stops working until the code is used, and it is an admin's and switched on. With no account by that email (the data was emptied, the last admin deleted) an admin's is made. |
| `nexus-music-server put-back [<copy>]` | Puts a copy of the data back (with nothing after it, lists the nightly copies). Every session ends; what the store says was taken away since is taken away again. |
| `nexus-music-server take-as-is` | Takes the data as it is when the store kept apart is lost or does not belong with it. Every session ends, and every account then waits for a new one-time code (`recover`). |

**Setting up with the log.** While the backend is not set up, each start writes a set-up code
to its log, good for fifteen minutes and once; a restart makes a new one. **Whoever can read
that log can read it while it lives.** If your logs are collected anywhere, use `set-up`
instead. Once the backend is set up, no code is written.

## Setting it up beside the proxy

**The backend has an HTTPS name of its own**, for example `nexus.example.org`, and nothing
else is served at that name: not the music server, not its web page, not any other service.
That name is what a person types into the app (design 7.1), and it is `NEXUS_PUBLIC_ORIGIN`
(`https://nexus.example.org`). **Never put the backend under the music server's name** (a
path such as `/nexus/` on the music server's host): a browser treats one name as one site, so
a page served there by anything else (the music server's own web app, say) could act in the
admin's page as the admin. The admin's page checks that a call comes from
`NEXUS_PUBLIC_ORIGIN`; that check protects nothing if another service shares it.

These are what the backend relies on; none of them can it check for itself.

- **Its port is reachable from the proxy alone.** Do not publish the container's port on
  the machine; put the container on a network it shares with the proxy, and nothing else.
- **`NEXUS_PROXY` says who the proxy is, in one of two ways.** If the proxy has a fixed
  address on that network, give that address. Otherwise give the proxy container's name on
  that network (for example `Nginx-Proxy-Manager-Official`): the backend looks the name up
  when connections come (at most every few seconds) and answers only the address it gives
  now, so a proxy started again at a new address is followed; while the name gives nothing,
  no one is answered. This relies on only the proxy's container answering to that name on
  that network, so keep the network to containers you run.
- **The backend reaches the music server inside, at `NEXUS_MUSIC_SERVER`** (for example
  `http://navidrome:4533` on a network it shares with the music server's container), and
  nowhere else. The address people use for the music server (its own HTTPS name on the proxy)
  is typed on the admin's page and only handed to devices: the backend needs no way out to it.
- **The rule sets `X-Nexus-Scheme` from `$scheme`, as below, and never to a fixed
  value.** The rule's line replaces whatever a caller sent under that name; without it, a
  caller's own header would reach the backend. `$scheme` is how the caller reached the
  proxy; behind a content network it is how the network reached it, which is still to be
  proven (the design's experiment E2b).

## The proxy's host (Nginx Proxy Manager)

A **new Proxy Host** for the backend's own name. Its whole path goes to the backend through
one `location /` written in the host's **Advanced** box; when that box has a `location /`, the
proxy leaves out the one it would make itself, so only these lines decide what reaches the
backend.

**Not as a Custom Location, and the container's name only in a variable.** Measured on Nginx
Proxy Manager 2.17.1: a custom location forwarded to a container's name is written so that
the name must be found when the proxy loads its configuration. While that container is
stopped, the proxy refuses every change to every host, and if the proxy starts then (a
reboot, say), it does not start at all, and no host it serves answers. Below, the name is in
a variable with a `resolver` line, so it is looked up for each request instead: a stopped
backend is then a 502 for this host and nothing more. **And nothing of the proxy's own
templates is named in the block** (no `$hsts_header`, no `include conf.d/include/...`): the
proxy renames them between versions, and a host naming one goes offline at its next
rewrite. The block writes its own HSTS header instead.

In Nginx Proxy Manager: **Hosts, Proxy Hosts, Add Proxy Host**.

1. On **Details**: **Domain Names** the backend's own name (`nexus.example.org`); **Scheme**
   `http`; **Forward Hostname / IP** the backend container's name (`nexus-server`) and
   **Forward Port** its port (`8080`), which the block below uses instead; **Cache Assets**
   off; **Block Common Exploits** off; **Websockets Support** off (the block passes one
   itself). **No Access List**: the backend has its own sign-in, and a list would refuse the
   apps (and no sign-in of another service in front of it, for the same reason).
2. On **SSL**: a certificate for that name; **Force SSL** on (plain HTTP is then only ever a
   redirect to HTTPS, and nothing reaches the backend by it); **HTTP/2 Support** as you like;
   **HSTS Enabled** off (the block writes its own, which a `location` with headers of its own
   needs).
3. On **Advanced**, exactly these lines, with the backend container's name in place of
   `nexus-server` and its port in place of `8080`:

       location / {
           resolver 127.0.0.11 valid=10s;
           set $nexus_up nexus-server;
           add_header Strict-Transport-Security "max-age=63072000; preload" always;
           proxy_set_header X-Nexus-Peer $realip_remote_addr;
           proxy_set_header X-Nexus-Scheme $scheme;
           proxy_set_header Cookie "";
           proxy_set_header Remote-User "";
           proxy_set_header Forwarded "";
           proxy_set_header True-Client-IP "";
           proxy_set_header CF-Connecting-IP "";
           proxy_set_header X-Real-IP "";
           proxy_set_header X-Forwarded-For "";
           proxy_set_header X-Forwarded-Proto "";
           proxy_set_header X-Forwarded-Scheme "";
           proxy_set_header X-Forwarded-Host "";
           proxy_set_header X-Forwarded-Port "";
           proxy_set_header X-Forwarded-Server "";
           proxy_set_header X-Forwarded-Ssl "";
           proxy_set_header X-Client-IP "";
           proxy_set_header X-Original-Forwarded-For "";
           proxy_set_header X-Cluster-Client-IP "";
           proxy_request_buffering on;
           proxy_intercept_errors off;
           proxy_cache off;
           proxy_no_cache 1;
           proxy_cache_bypass 1;
           access_log off;
           error_log /dev/null crit;
           client_max_body_size 1m;
           client_body_buffer_size 1m;
           client_body_in_file_only off;
           proxy_http_version 1.1;
           proxy_set_header Upgrade $http_upgrade;
           proxy_set_header Connection $http_connection;
           proxy_pass http://$nexus_up:8080;
       }

4. **Save.** Then check that `https://nexus.example.org/nexus/api/hello` answers (the
   backend's id), and that the music server's own name does not.

What the lines do: `127.0.0.11` is Docker's own name service, so the proxy and the backend
must share a network Docker made (not the default bridge); `proxy_pass` has no path, so the
backend sees the address exactly as the caller wrote it and can refuse it (anything not
under `/nexus/` is refused by the backend). **The only headers that say who and how are the
rule's own two**, `X-Nexus-Peer` and `X-Nexus-Scheme`, which replace whatever a caller sent
under those names; every other header by which a caller could claim an address, a scheme, a
host or a name (`X-Real-IP`, `X-Forwarded-For` and the rest above), and its cookies, are
emptied and never reach the backend (measured: without these lines the caller's own values
arrive). The three lines after them keep the proxy from caching, or from putting answers of
its own in place of the backend's. The last three before `proxy_pass` pass a WebSocket
(stage 2). The HSTS line tells browsers to come to this name by HTTPS only.

**`client_body_buffer_size 1m;` and `client_body_in_file_only off;` are required** (design 7.2):
the proxy takes a body whole before the backend sees it, and with a body allowed to be larger
than its memory for one it wrote a body carrying a password to a file of its own (measured).
With these, a body either fits in the proxy's memory or is refused; it is never put on its
disk. (Measured at 4 KiB on 2.17.1; to be checked again on the owner's 2.16.0 before anything
of his is changed, E2b. If it does not hold there, the calls that carry a password get a
`location` of their own at 4 KiB.) **The backend itself takes no more than 4 KiB on any call
that carries a password**, refused from its `Content-Length` before a byte of the body is
read. Where the TLS is ended (the proxy, or a content network in front of it) everything can
be read, passwords included: have that edge refuse plain HTTP outright.

**`error_log /dev/null crit;` is required.** A backend that is down, restarting or too slow
otherwise has the proxy write, for every request, a line to its error log holding the
caller's address (as the caller claims it) and the whole request line with its query
(measured). **What is given up:** that host's own errors are no longer in the proxy's log;
the backend's health check and its own log say whether it is up.

One thing the rule cannot stop: a request the proxy refuses itself before it chooses a
location (a header too large for it, say) is written to the host's own log.

## What reaches it, and what it answers

- **Only connections from `NEXUS_PROXY`.** Anyone else is closed without a byte read or
  written; except this machine's loopback, which may ask `GET /nexus/api/hello` and nothing
  else, for the container's health check (`nexus-music-server health`).
- **One request per connection.** Its head is read by the backend itself, at most 16 KiB and
  64 headers within 3 s of the connection (a sign-in's body too, which the proxy sends with
  it), before anything else looks at it and before it takes one of the 64 places it is served
  in.
- **16 of those places are kept for people already known**: a request whose session the
  backend has, a device its account knows signing in, or a live one-time code. A flood of
  strangers can take the other 48, never these, so the people signed in keep being served.
  (The owner's own way in during a flood is `recover`, which needs no HTTP.)
- **The gate:** the request must carry exactly one `X-Nexus-Scheme: https`. Whatever fails
  before or at the gate from the proxy (no mark, a head too large, too slow or not a head,
  every place taken) is answered with **one fixed bare 400**, the same bytes for every
  cause, so that the proxy hands the caller a plain answer and logs nothing.
- **Then:** the target exactly as it came, in plain form (400); the method, `GET` alone for
  `hello` (any other is 405, a path there is not is 404); a body counted by
  `Content-Length` alone, at most the call's own ceiling (1 KiB for most calls, 4 KiB for
  one that carries a password, 1 MiB at most; no chunks, no `Expect`: the proxy sends every
  body counted). Each refusal is a bare answer, and the body is not read before it.
- **Then whose session it is, before a byte of the body is read.** The admin's page has a
  session of its own kind, and an app's session can do nothing the page does, nor the
  reverse: every call under `/nexus/api/admin/` (the grant included) is the page's alone;
  every other call that needs a session is an app's alone (devices, force-out,
  change-password, the music login, events, Diagnostics, requests); signing in, setting up
  and `hello` need none; signing out is either's. A call with no session, or one of the
  other kind, is refused at once ("session ended", 401; "not allowed", 403), the same bytes
  on every call. After the data has been put back, a session the data does not have is
  judged from the body instead, so that a device from before is told "the data was put back"
  and sends its events again.
- Every answer has `Cache-Control: no-store`, sets no cookie, and nothing about a request
  is written to the log.

## The music login (S9)

**Two music-server addresses, and the backend connects to one.**

- **`NEXUS_MUSIC_SERVER`**, a container setting (above): the music server as the container
  reaches it. It is the only address the backend ever connects to.
- **The address people use**, set on the admin's page (`/nexus/api/admin/set-music-server`,
  with a grant): the HTTPS address devices reach the music server at, for example
  `https://music.example.org`. Only its form is checked; **the backend never connects to it**,
  it only hands it to devices. A wrong one shows at once on a device ("the music server can't
  be reached"), and is corrected on the page.

An admin types a person's music-server name and password once (the page's
`/nexus/api/admin/set-music-login`, with a grant for that account). The backend asks the music
server `ping` then `getUser` at `NEXUS_MUSIC_SERVER`, each a POST of a form it builds, following
no redirect, within 8 s for both, at most two such checks at once, and keeps the login only if
it answers with that very name. It is sealed (XChaCha20-Poly1305, a key of its own in the store
kept apart) and bound to this backend, the account, the address it was checked at, the name and
the login's number, so a copy of the data holds nothing usable, and an older login put back does
not open. **Changing `NEXUS_MUSIC_SERVER`** makes every login "cannot be used now" until it is
typed again. (In this build the seal also names the address people use, so changing that too
asks for every login to be typed again; the shared library's next round binds it to
`NEXUS_MUSIC_SERVER` alone, and the address people use can then be corrected freely. A login
cannot be set while the address people use is unset.)

A device is never handed the password: at sign-in and at `/nexus/api/music-login` it gets a
salt made then and its token, with the address people use, or "none set yet", or "cannot be
used now" (the page then asks for it to be typed again). The admin's page is never handed one.
"Wrong name or password" (403 `music_login_refused`) is told apart from "could not reach it"
(503); neither touches the login the account had. **Forcing a device out does not change this
login**: that device still holds a salt and token the music server accepts until that user's
password is changed on the music server.

`/nexus/api/hello` answers, for example,
`{"has":["accounts"],"id":"0123456789abcdef0123456789abcdef","nexus":"0.1.0","speaks":[1,1]}`.

Every other call is a `POST` under `/nexus/api/` with a JSON body of the shared library's
shapes (`nexus_core::nexus::account`): `signin`, `signout`, `devices`, `force-out`,
`change-password`, `music-login`. Each answers 200 (done), 400 (malformed), 401 (not signed in:
this session has ended, or the data was put back), 403 (not allowed: a wrong email or password
is one answer, whichever it was), 429 (busy, come back) or 503 (not dealt with).

**Asked twice (12.2), in this build:** the record of request numbers is not kept yet, so
every call that changes something is safe to repeat by its own nature: a sign-out, or a note,
for a session already ended ends nothing more; a force-out names the session it ends, so sent
again after that device has signed in afresh it ends nothing; a password change sent again is
refused, because the password it proves is no longer the present one; a sign-in is never
answered twice, and a lost answer is a new sign-in.
The id is made at random at first start and kept for as long as `NEXUS_STATE/id` is: a
device asks its person again if it ever changes. Two copies started at once on the same
store end with one id, which both serve.

## Known limits of this build

- **`put-back` is run by hand, with the backend stopped, and its result checked**: it says what
  it did; start the backend and look at the log's first line.
- **Two backends sharing one host's folders is unsupported**: each backend's `NEXUS_DATA` and
  `NEXUS_STATE` are its own.
- If the data in place cannot be read as data, `put-back` refuses (it keeps one checked copy of
  what it replaces, or replaces nothing): move the damaged file out of `NEXUS_DATA` by hand.
- A `put-back` that has completed, run again with the same copy, is a second put-back.
- **The general record of request numbers (design 12.2) is not in this build**: every call
  that changes something is safe to repeat by its own nature (above).
