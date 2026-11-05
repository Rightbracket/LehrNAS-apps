# Publish your own catalog

Any URL that serves a TOML file in this format is a catalog. An admin
adds it under **Apps › Bazaar › Catalogs** by URL; the box fetches it
first and shows the name, the entry count and whether it is signed before
adding. Its entries then appear in the same bazaar, marked with the
catalog's name and *unsigned* unless you sign.

1. Write `catalog.toml` (any name) with a `[channel]` table and your
   `[[apps]]` entries; put any compose files beside it.
2. Run `python3 tools/check.py catalog.toml` from a copy of this repository: it
   applies the same rules a box does.
3. Serve it over HTTPS, raw: a public GitHub repository's raw URL works.
4. Optionally sign: `cosign generate-key-pair`, commit the public half
   beside the file as `[channel] key = "your-key.pub"`, and sign every
   change with `cosign sign-blob --key your.key --new-bundle-format=false
   --use-signing-config=false --tlog-upload=false --output-signature
   catalog.toml.sig catalog.toml`. The box fills the key in from the file
   the first time an admin adds the catalog and shows its fingerprint, for
   them to compare with the key you publish elsewhere; it keeps that key
   and refuses any later fetch that doesn't verify with it.

A catalog that names a key must carry its signature; one without a key is
shown as unsigned, which is fine for a list you keep for yourself.

## The entry format

```toml
[channel]
name = "Mara's homelab recipes"       # shown on every entry's chip
url = "https://example.org/lehr/catalog.toml"
key = "mara.pub"                      # optional; the file beside the catalog

[[apps]]
id = "jellyfin"                       # the app's name on the box: a-z, 0-9, dashes
name = "Jellyfin"
summary = "Streams your films and music to any screen in the house."
group = "Media"                       # your own word for what it is for
version = "jellyfin-10.10.7"          # the release this entry describes
released = "2026-11-01"
image = "docker.io/jellyfin/jellyfin" # the one-image shorthand: repository, no tag
[apps.digests]
list = "sha256:…"                     # the manifest list: what the box pins
amd64 = "sha256:…"                    # each architecture's own, for readers
arm64 = "sha256:…"
[apps.requirements]                   # the box warns when short, never blocks
mem_mib = 2048
cpus = 2
gpu = "any"                           # optional
[apps.permissions]                    # what the admin allows, upfront
ports = ["8096:8096/tcp"]             # LAN port:container port; the box publishes it
shares = "read-only, the media share"
internet = true
devices = ["/dev/dri"]                # optional
host_network = false
capabilities = []
[[apps.data]]                         # named, permanent storage locations
name = "config"
bytes = 1073741824                    # a size hint
[[apps.data]]
name = "cache"
bytes = 10737418240
[apps.user]
kind = "default app user"             # or "dedicated"
[[apps.seed]]                         # files written before the first start
location = "config"
path = "network.xml"
text = """<NetworkConfiguration>…</NetworkConfiguration>"""
note = "Point it at the media share on first start."
```

An app of several containers carries its compose file instead of
`image`, either inline or by path, and one `[[apps.images]]` row per
service. The compose file is the truth: the box reads what it opens and
mounts and shows anything the entry didn't declare. Every service's image
is pulled by its pinned digest, never by the tag the compose names, so a
service without a row refuses the entry.

```toml
[[apps]]
id = "paperless"
name = "Paperless-ngx"
summary = "Scans, indexes and files every document you feed it."
group = "Office"
version = "paperless-2.19.5"
released = "2026-10-20"
compose_path = "apps/paperless/compose.yaml"   # beside the catalog file
compose_sha256 = "…"                           # of that file's bytes
# or: compose = """ ...the file inline... """  (at most 64 KiB)
[[apps.images]]
service = "webserver"
image = "ghcr.io/paperless-ngx/paperless-ngx"
list = "sha256:…"
[[apps.images]]
service = "broker"
image = "docker.io/valkey/valkey"
list = "sha256:…"
```

Rules the box applies (and `tools/check.py` checks): `id` is lower-case
letters, digits and dashes, at most 32; `name`, `summary`, `version` and
`released` are required; exactly one of `image`, `compose` or
`compose_path`; every digest is `sha256:` and 64 hex digits; a
`compose_path` is relative, beside the catalog, with its `compose_sha256`;
a one-image entry's ports are `HOST:CONTAINER` with an optional `/tcp`
or `/udp`, since the box publishes exactly those (a compose entry's ports
come from its file, and the entry's list is the author's summary); a
seed's `location` names one of the entry's data locations.
