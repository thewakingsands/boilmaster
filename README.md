# Boilmaster (ffcafe fork)

Web service for Final Fantasy XIV game data and asset discovery, forked from [ackwell](https://github.com/ackwell/boilmaster) by FFCafe.

## Notes for API Users

We maintain an instance at <https://xivapi-v2.xivcdn.com/> and provide Chinese-accelerated access.

* Game data supports Chinese, Japanese, English, German, and French.
* Data requests always use the newest locally downloaded release. The `version` query parameter is ignored.
* `GET /api/version` (also `/api/versions`) lists all retained local releases, newest first, and reports the current update job.
* Asset retrieval endpoints remain disabled.

## Installation

### Docker

```yml
services:
  boilmaster:
    image: ghcr.io/thewakingsands/boilmaster:latest
    container_name: boilmaster
    environment:
      BM_UPDATE_TOKEN: ${BM_UPDATE_TOKEN}
    volumes:
      - ${PWD}/persist:/app/persist
    ports:
      - 8080:8080
    restart: unless-stopped
```

### Game data and versions

At startup Boilmaster checks the [latest stable ixion release](https://github.com/thewakingsands/ixion/releases), downloads its `merged-{version}.zip`, validates the version file and SqPack data, then activates it. An existing local release remains available if the check or download fails. A first startup without cached data must successfully download a release before serving.

Set `BM_GAME_DIRECTORY` to a writable persistent directory (default `game`; Docker default `/app/persist/game`). No manually mounted game installation is required. Each downloaded release has its own directory and `release.json` metadata. The newest 10 releases are retained; temporary downloads are removed on completion or failure. Existing unmanaged files in this directory are not imported or deleted.

Public version keys are the release titles, for example `20260908-6d044b4`. The separate `version` field comes from the asset filename, for example `2026.09.01.0000.0000`. Releases with the same game version but different release titles are distinct versions. Only the newest local release is announced to search ingestion. Search indexing runs asynchronously after activation, so new-version searches may be temporarily unavailable while indexing completes. Search cursors from an older release must be restarted after an update.

The `/admin` routes are temporarily disabled. Use `GET /api/version` to inspect local releases and update status.

### Triggering an update

Set `BM_UPDATE_TOKEN` in the server environment. If it is absent or empty, update requests are rejected. Trigger an asynchronous update with either:

```sh
curl -X POST -H "Authorization: Bearer $BM_UPDATE_TOKEN" https://your-host/api/version
# Compatible with the token-query flow in ixion/scripts/trigger-xivstrings-update.mjs:
curl -X POST "https://your-host/api/version?token=$BM_UPDATE_TOKEN"
```

The response is `202 Accepted` with `update.state` set to `running`. Poll `GET /api/version` until the same `update.startedAt` job reaches `success` or `error`. Concurrent triggers return the currently running job. `update.updated` is `false` when the local release is already current; errors appear in `update.error`. `startedAt` and `finishedAt` are Unix nanosecond strings and can be compared directly as job identifiers. GET is public; only POST requires the update token. The application request span logs the URL path without the token query string.

Example completed response:

```json
{
  "key": "20260908-6d044b4",
  "version": "2026.09.01.0000.0000",
  "versions": [
    {
      "key": "20260908-6d044b4",
      "version": "2026.09.01.0000.0000",
      "published_at": "2026-09-08T12:54:01Z",
      "names": ["latest"]
    }
  ],
  "update": {
    "state": "success",
    "startedAt": "1788872100000000000",
    "finishedAt": "1788872160000000000",
    "updated": true,
    "error": null
  }
}
```

### Switching supported languages

The merged archive contains data from multiple servers. Set `BM_READ_LANGUAGE_EXCLUDE` to control the exposed languages, for example `[chs,ko]` for global languages, `[ja,en,de,fr,ko]` for Chinese only, or `[]` for all available languages.

Test language support with:

```
/api/sheet/Item/1?fields=Name@lang(chs),Name@lang(de),Name@lang(en),Name@lang(fr),Name@lang(ja)
```

### Release lifecycle tests

```powershell
$env:BM_TEST_ARCHIVE = 'C:\path\to\merged-2026.02.20.0000.0000.zip'
cargo test -p bm_data --lib -- --include-ignored
cargo test -p bm_http --lib
```

The fixture test uses a local HTTP server and temporary directories to exercise download, activation, duplicate triggers, failures, retention and offline restarts.
