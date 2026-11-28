# The Nexus server

One small service of our own, in one container, beside the music server. The design is
`docs/backend-design.md` on its own branch. It answers `hello` (S1), keeps its data and a
store kept apart (S3), and signs people in with an email and a password (S8). The music login
(S9) and the admin's page's working calls (S10b) are not in yet: every sign-in says there is
no music login yet.

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
| `NEXUS_MUSIC_SERVER` | The music server as the container reaches it, for example `http://navidrome:4533`. **Must be given.** (Not used yet.) |
| `NEXUS_PROXY` | The one IP address the proxy connects to this container from, for example `172.18.0.2`. **Must be given.** Nothing else is answered. |
| `NEXUS_DATA` | The data folder: one SQLite file, `nexus.db`, and its nightly copies in `copies/`. `/data` unless given. On a disk of the machine's own, not a network folder. |
| `NEXUS_STATE` | The folder of the store kept apart: the backend's id (`id`), its restore number, its keys, and what must never go backwards when the data does (`kept`). `/state` unless given. Not the data folder, inside it or around it, through a link or a mount either. **Never copied or put back with the data.** |
| `NEXUS_LISTEN` | `0.0.0.0:8080` unless given. Not port 0. The health check asks on this machine's loopback, so listen on every address or on loopback. |
| `NEXUS_PUBLIC_ORIGIN` | The admin's page's address as a browser has it: `https://` and the name, and `:port` only if it is not 443, nothing after it, for example `https://nexus.example.org`. The page's sign-in and set-up are taken only from it. Unset, they are refused, and the log says so once at the start. |

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

These are what the backend relies on; none of them can it check for itself.

- **Its port is reachable from the proxy alone.** Do not publish the container's port on
  the machine; put the container on a network it shares with the proxy, and nothing else.
- **The proxy's address on that network is fixed**, and it is `NEXUS_PROXY`: a container's
  address can change when it is started again, and then nothing is answered.
- **The rule sets `X-Nexus-Scheme` from `$scheme`, as below, and never to a fixed
  value.** The rule's line replaces whatever a caller sent under that name; without it, a
  caller's own header would reach the backend. `$scheme` is how the caller reached the
  proxy; behind a content network it is how the network reached it, which is still to be
  proven (the design's experiment E2b).

## The proxy's rule

**Not as a Custom Location.** Measured on Nginx Proxy Manager 2.17.1: a custom location
forwarded to a container's name is written so that the name must be found when the proxy
loads its configuration. While that container is stopped, the proxy refuses every change to
every host, and if the proxy starts then (a reboot, say), it does not start at all, and no
host it serves answers, the music server's included. So the rule is written in the music
server's host's own **Advanced** box, where the name is looked up for each request instead:
a stopped backend is then a 502 for `/nexus/` and nothing more.

In Nginx Proxy Manager: **Hosts, Proxy Hosts**, the music server's host, **Edit**.

1. On **Details**: **Cache Assets** off.
2. On **Custom Locations**: if there is a `/nexus/` location, delete it. There must be only
   this one.
3. On **Advanced**, add exactly these lines below anything already there, with the backend
   container's name in place of `nexus-server` and its port in place of `8080`:

       location /nexus/ {
           resolver 127.0.0.11 valid=10s;
           set $nexus_up nexus-server;
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

4. **Save.**

What the lines do: `127.0.0.11` is Docker's own name service, so the proxy and the backend
must share a network Docker made (not the default bridge); `proxy_pass` has no path, so the
backend sees the address exactly as the caller wrote it and can refuse it. **The only
headers that say who and how are the rule's own two**, `X-Nexus-Peer` and `X-Nexus-Scheme`,
which replace whatever a caller sent under those names; every other header by which a
caller could claim an address, a scheme, a host or a name (`X-Real-IP`, `X-Forwarded-For`
and the rest above), and its cookies, are emptied and never reach the backend (measured:
without these lines the caller's own values arrive). The three lines after them keep the
proxy from caching, or from putting answers of its own in place of the backend's, whatever
the host is set to. The last three before `proxy_pass` pass a WebSocket (stage 2).

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

**An Access List on the host does not cover `/nexus/`** (measured: with one, the music
server's own paths asked for the password and `/nexus/` did not). The backend has its own
gate, and at a later stage its own sign-in, so it does not need one. To have the list apply
here too, copy into this block, above `resolver`, the lines the proxy writes for it in the
host's own `location /` (in `/data/nginx/proxy_host/<the host's number>.conf`): for a list
with a user, `auth_basic "Authorization required";`, `auth_basic_user_file
/data/access/<the list's number>;` and `satisfy all;` (or `any`, as the list says), and for
a list with addresses, its `allow` lines and `deny all;` (measured: with them, `/nexus/` asks
for the password too). The apps are then refused here as they are on the rest of the host,
unless the list lets them by.

**`error_log /dev/null crit;` is required.** A backend that is down, restarting or too slow
otherwise has the proxy write, for every request, a line to its error log holding the
caller's address (as the caller claims it) and the whole request line with its query
(measured). **What is given up:** that location's own errors are no longer in the proxy's
log; the backend's health check and its own log say whether it is up.

One thing the rule cannot stop: a request the proxy refuses itself before it chooses a
location (a header too large for it, say) is written to the host's own logs, as for any
other path of the music server.

## What reaches it, and what it answers

- **Only connections from `NEXUS_PROXY`.** Anyone else is closed without a byte read or
  written; except this machine's loopback, which may ask `GET /nexus/api/hello` and nothing
  else, for the container's health check (`nexus-music-server health`).
- **One request per connection.** Its head is read by the backend itself, at most 16 KiB and
  64 headers within 10 s, before anything else looks at it.
- **The gate:** the request must carry exactly one `X-Nexus-Scheme: https`. Whatever fails
  before or at the gate from the proxy (no mark, a head too large, too slow or not a head,
  every connection taken) is answered with **one fixed bare 400**, the same bytes for every
  cause, so that the proxy hands the caller a plain answer and logs nothing.
- **Then:** the target exactly as it came, in plain form (400); the method, `GET` alone for
  `hello` (any other is 405, a path there is not is 404); a body counted by
  `Content-Length` alone, at most the call's own ceiling (1 KiB for most calls, 4 KiB for
  one that carries a password, 1 MiB at most; no chunks, no `Expect`: the proxy sends every
  body counted). Each refusal is a bare answer, and the body is not read before it.
- Every answer has `Cache-Control: no-store`, sets no cookie, and nothing about a request
  is written to the log.

`/nexus/api/hello` answers, for example,
`{"has":["accounts"],"id":"0123456789abcdef0123456789abcdef","nexus":"0.1.0","speaks":[1,1]}`.

Every other call is a `POST` under `/nexus/api/` with a JSON body of the shared library's
shapes (`nexus_core::nexus::account`): `signin`, `signout`, `devices`, `force-out`,
`change-password`, `music-login`. Each answers 200 (done), 400 (malformed), 401 (not signed in:
this session has ended, or the data was put back), 403 (not allowed: a wrong email or password
is one answer, whichever it was), 429 (busy, come back) or 503 (not dealt with).
The id is made at random at first start and kept for as long as `NEXUS_STATE/id` is: a
device asks its person again if it ever changes. Two copies started at once on the same
store end with one id, which both serve.
