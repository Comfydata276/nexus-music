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
| `NEXUS_PROXY` | The one IP address the proxy connects to this container from, for example `172.18.0.2`. **Must be given.** Nothing else is answered. Give the proxy a fixed address on the network they share, since a container's address can change when it is started again. |
| `NEXUS_DATA` | The data folder. `/data` unless given. Not used yet. |
| `NEXUS_STATE` | The folder of the store kept apart. `/state` unless given. Not inside the data folder. The backend's id is kept here, in `id`. |
| `NEXUS_LISTEN` | `0.0.0.0:8080` unless given. |

A setting that is wrong or missing is said by name as it starts, and it stops.

## What reaches it

Only connections from `NEXUS_PROXY`, whose requests the proxy marks with exactly one
`X-Nexus-Scheme: https` (the proxy's rule is in the design, 6.2). Anything else is closed
without a byte. The container's own health check (`nexus-music-server health`) asks `hello`
on its loopback address, which may ask that and nothing else.

`/nexus/api/hello` answers, for example,
`{"nexus":"0.1.0","id":"0123456789abcdef0123456789abcdef","speaks":[1,1],"has":[]}`.
The id is made at random at first start and kept for as long as `NEXUS_STATE/id` is: a
device asks its person again if it ever changes.
