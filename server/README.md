# The Nexus server

One small service of our own, in one container, beside the music server. The design is
`docs/backend-design.md` on its own branch. **This is its first slice (S1): it answers
`GET /nexus/api/hello` and nothing else.** It holds no data, no sessions and nothing secret.

## Building and testing

Built apart from the app, as the relay is:

    cargo test --manifest-path server/Cargo.toml
    docker build -f server/Dockerfile -t nexus-music-server .

`tests/e6.rs` is the design's experiment E6, over real sockets on loopback. Every bound it
holds the backend to is a number in `src/bounds.rs`.

## Settings

| Setting | |
|---|---|
| `NEXUS_MUSIC_SERVER` | The music server as the container reaches it, for example `http://navidrome:4533`. **Must be given.** (Not used yet.) |
| `NEXUS_PROXY` | The one IP address the proxy connects to this container from, for example `172.18.0.2`. **Must be given.** Nothing else is answered. |
| `NEXUS_DATA` | The data folder. `/data` unless given. Not used yet. |
| `NEXUS_STATE` | The folder of the store kept apart. `/state` unless given. Not the data folder, inside it or around it, through a link or a mount either. The backend's id is kept here, in `id`. |
| `NEXUS_LISTEN` | `0.0.0.0:8080` unless given. Not port 0. The health check asks on this machine's loopback, so listen on every address or on loopback. |

A setting that is wrong or missing is said by name as it starts, and it stops.

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
           proxy_set_header Cookie "";
           proxy_set_header X-Nexus-Peer $realip_remote_addr;
           proxy_set_header X-Nexus-Scheme $scheme;
           proxy_set_header Remote-User "";
           proxy_set_header Forwarded "";
           proxy_set_header True-Client-IP "";
           proxy_set_header CF-Connecting-IP "";
           proxy_set_header X-Forwarded-Host "";
           proxy_no_cache 1;
           proxy_cache_bypass 1;
           access_log off;
           error_log /dev/null crit;
           client_max_body_size 1m;
           proxy_http_version 1.1;
           proxy_set_header Upgrade $http_upgrade;
           proxy_set_header Connection $http_connection;
           proxy_pass http://$nexus_up:8080;
       }

4. **Save.**

What the lines do: `127.0.0.11` is Docker's own name service, so the proxy and the backend
must share a network Docker made (not the default bridge); `proxy_pass` has no path, so the
backend sees the address exactly as the caller wrote it and can refuse it; the `Cookie` and
the other emptied headers never reach it; the last three lines pass a WebSocket (stage 2).

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
  `Content-Length` alone, at most 1 MiB (no chunks, no `Expect`: the proxy sends every body
  counted). Each refusal is a bare answer, and the body is not read before it.
- Every answer has `Cache-Control: no-store`, sets no cookie, and nothing about a request
  is written to the log.

`/nexus/api/hello` answers, for example,
`{"has":[],"id":"0123456789abcdef0123456789abcdef","nexus":"0.1.0","speaks":[1,1]}`.
The id is made at random at first start and kept for as long as `NEXUS_STATE/id` is: a
device asks its person again if it ever changes. Two copies started at once on the same
store end with one id, which both serve.
