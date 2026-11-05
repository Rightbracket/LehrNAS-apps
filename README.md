# LehrNAS apps

The official LehrNAS app catalog, and the code of the official apps.

**Any URL serving a TOML file in this format is a catalog:** see
[publish your own](docs/publish-your-own-catalog.md).

| Path | What it is |
|---|---|
| `official.toml` | The catalog: a `[channel]` table and one `[[apps]]` table per app. Written by each app's release, never by hand |
| `official.toml.sig` | The app-channel key's signature over the file's bytes (`cosign sign-blob`) |
| `app-channel.pub` | The key's public half. Every LehrNAS box ships the same bytes and verifies the catalog with them before reading a single entry |
| `apps/<id>/` | One official app, standing alone: its entry, image, code and tests |
| `tools/` | `check.py` checks a catalog by the box's rules; `check_template.py` checks every app's entry |
| `docs/` | [Publishing your own catalog](docs/publish-your-own-catalog.md) and [adding an app](docs/adding-an-app.md) |

## The apps

| App | What it does |
|---|---|
| `fileferry` | Moves an old NAS's files into LehrNAS shares, verified from the drive. In the catalog; its code isn't here yet |
| [`gha-runner`](apps/gha-runner/README.md) | Runs your own repositories' GitHub Actions jobs on the box, one at a time. Not published yet |

## How a box uses it

A LehrNAS box has this catalog built in. It fetches the file by its URL
when the admin opens **Apps › Bazaar** and once a day for the update
check, verifies the signature with the key it ships, and keeps the last
copy that verified. Each entry shows in the bazaar with its provenance
(*LehrNAS official · signed*), its requirements checked against the box,
and its permissions. An install pulls every image by the digest the entry
pins, never by a tag, so what runs is what was released and signed.

Nothing here reaches a box by itself: a new entry is an *update
available* line in the box's Apps list, applied only when the admin
reviews it.

## Adding an app

[Adding an app](docs/adding-an-app.md) has the folder, the rules and the steps. Only
official apps live here. This repository takes no pull requests: its branches are
reviewed and merged by the project's lead.
