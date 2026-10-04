# Boilmaster (ffcafe fork)

Web service for Final Fantasy XIV game data and asset discovery, forked from [ackwell](https://github.com/ackwell/boilmaster) by FFCafe.

## Notes for API Users

We maintain an instance at <https://xivapi-v2.xivcdn.com/> and provide Chinese-accelerated access.

* Game data supports Chinese, Japanese, English, German, and French.
* Data requests always use the newest locally downloaded release. The `version` query parameter is ignored.
* `GET /api/version` (also `/api/versions`) lists all retained local releases, newest first, and reports the current update job.
* Asset retrieval can be enabled against ixion's binary index in MinIO, independently of EXD releases (see below).

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

### Administration and documentation sync

The admin dashboard is available at `/admin/` when enabled. It shows retained
data releases, the active data version and update status; currently loaded asset
indexes, entry counts, fingerprints and refresh status; and the active synced
documentation release. It offers data update, asset reload and documentation sync
buttons. It does not offer version rollback or deletion. Asset versions are read
from the in-process index cache, not by listing MinIO objects.

Configure the following environment variables (there are no default credentials):

```text
BM_HTTP_ADMIN_ENABLED=true
BM_HTTP_ADMIN_AUTH_USERNAME=<administrator name>
BM_HTTP_ADMIN_AUTH_PASSWORD=<strong unique password>
BM_HTTP_ADMIN_DOCS_REPOSITORY=thewakingsands/xivapi-v2
BM_HTTP_DIRECTORY=/app/static
```

The dashboard is disabled by default and enabling it without nonempty credentials
fails startup. Serve it behind HTTPS; restrict `/admin` at the reverse proxy to
trusted operators where possible. Basic authentication protects pages, scripts,
status and update endpoints. POST actions additionally require a per-process CSRF
token obtained from the authenticated page. Admin responses are not cacheable.
This login is separate from the existing `BM_UPDATE_TOKEN` API integration.
The dashboard uses local scripts/styles and does not depend on an external CDN.

Data updates use the existing asynchronous ixion updater and join an already
running job. Asset reloads use the same atomic reader as the hourly refresh;
failed validation keeps the previous index. When `BM_ASSET_VERSION` pins a version,
the dashboard shows the pin and disables manual reload instead of overriding it.

Documentation synchronization anonymously downloads `docs.zip` and
`docs.zip.sha256` from the configured public repository's latest stable GitHub
Release. Actions artifacts are not used because their download endpoint requires
authentication. The documentation repository publishes these assets when `main`
builds successfully; publishing does not automatically synchronize running servers.

For server-side sync, mount a **writable, persistent** directory at `/app/static`
(or `BM_HTTP_DIRECTORY`). The existing static files continue to work before the
first sync. New releases are checksum-checked, size-limited and safely extracted
under `.boilmaster-docs/<installation-id>/site`; a persisted `current.json` pointer
is atomically replaced only after validating the Chinese, English and root entry
pages. HTTP requests then use the new directory without restarting, and the
selection survives restart. Failed syncs keep the previous site. Old installations
are retained (there is no automatic disk cleanup or rollback UI). Avoid pointing
an external static server at the mount root after syncing: only boilmaster follows
the managed pointer and blocks direct access to its metadata directory. Share a
volume with only one updating boilmaster process. A read-only mount remains valid
for manual deployment, but the sync button will report a write failure.

Documentation updates do not invalidate existing browser/CDN caches. Clear stale
HTML caches if an immediate switch is needed. Before the first documentation
Release is published, the sync action reports an error and leaves the site intact.

### Assets and migration from the Go service

WebP and AVIF source objects are supported. AVIF decoding uses `avif-decode`/`rav1d` in Rust, without a system dav1d library. Alpha and high-bit-depth channels are preserved during decoding; JPEG and WebP responses are converted to 8-bit, while PNG can retain 16-bit channels. Map composition follows upstream's 8-bit RGBA behavior. Raw AVIF delivery through legacy `/i/` does not require decoding.

Set these environment variables (or the equivalent `[asset]` settings in `boilmaster.toml`):

```text
BM_ASSET_ENABLED=true
BM_ASSET_ENDPOINT=https://minio.example.com
BM_ASSET_BUCKET=your-bucket
BM_ASSET_ACCESSKEY=read-only-access-key
BM_ASSET_SECRETKEY=read-only-secret-key
BM_ASSET_PREFIX=ixion/ui/sdo
BM_ASSET_REGION=us-east-1
BM_ASSET_CACHE=/app/persist/assets
```

The S3 endpoint must include its scheme. The account only needs object GET access beneath the configured prefix; the reader does not list, upload or delete objects. These settings replace the Go server's `MINIO_*` variables. The default is disabled so existing EXD-only installations continue to start; asset requests then return 503.

At startup, the reader selects `current.json.lastValidIndex` (falling back to `ffxiv` only when the former is absent/empty) and loads `patches/<version>/assets.bin`. `BM_ASSET_VERSION` can pin an explicit version without reading `current.json`. Missing or corrupt binary indexes fail startup: there is no fallback to `icons.json` or another version. Publish the binary index with ixion before enabling the service. Without a version pin, the service reloads `current.json` and its binary index every hour (first check one hour after the refresh loop starts), including index replacements under the same version. A validated snapshot is switched atomically; failed refreshes keep serving the last valid snapshot and are retried at the next hourly check. Existing requests retain their original snapshot. A configured `BM_ASSET_VERSION` disables these reloads. Both MinIO and local-directory storage support this behavior. Images are fetched from unchanged `assets/<first-two-hash-digits>/<hash>.<format>` objects; only these immutable image objects are cached on disk.

Supported routes:

* `GET /api/asset?path=ui/icon/051000/051474_hr1.tex&format=png`
* `GET /api/asset/ui/icon/051000/051474_hr1.tex?format=webp`
* `GET /api/asset/map/s1d1/00?format=png` (defaults to `jpg`)
* `GET /i/051000/051474_hr1.png` (legacy Go compatibility)

The file routes prefer the stored encoding when the request's `Accept` header allows it, using `format` only as a fallback. Map composition uses the requested encoding and the main and optional background textures, including upstream's precomposed-map detection. Legacy `/i/` returns the original stored WebP/AVIF bytes with their actual content type, despite its `.png` suffix, and preserves the old ID-based lookup regardless of the supplied six-digit directory. These routes support ETag, conditional GET and HEAD. File responses include `Vary: Accept`, including on 304 responses, and ETags distinguish actual output content types. Errors are not marked cacheable. ETags also include a digest of the entire index so replacing a same-version index invalidates response validators after a successful reload. Browser/CDN responses already cached with `max-age` remain usable until they expire or are explicitly purged; a server reload does not push invalidations to clients.

The file endpoints also accept `format=avif` for **passthrough only**: if the indexed source is AVIF, the response is byte-identical to the stored object, with `Content-Type: image/avif` and an `.avif` filename. No decoding or re-encoding takes place. Falling back to AVIF for a non-AVIF source returns 400; it is not converted to AVIF. If `Accept` allows that non-AVIF stored type, it is returned unchanged instead, ignoring `format=avif`. To retrieve an AVIF map source texture unchanged, use `/api/asset?path=ui/map/s1d1/00/s1d100_m.tex&format=avif`. The composed-map endpoint does not accept AVIF, since returning its uncomposed source would change the endpoint's meaning.

When `format` is omitted, `/api/asset?path=...` and `/api/asset/<game-path>` return the **stored object unchanged**, regardless of `Accept`, with its actual `Content-Type` and filename extension (normally WebP or AVIF). No decoding or re-encoding takes place. When a supported `format` is supplied, an `Accept` match for the stored type also returns those raw bytes, even if another type has a higher quality value. Matching supports exact types, `image/*`, `*/*`, and quality weights; a more specific `q=0` excludes that stored type. Missing, empty or non-matching `Accept` falls back to `format` (jpg/png/webp conversion, or AVIF passthrough only). For example, `format=png` with `Accept: image/avif,image/webp,*/*;q=0.8` returns stored WebP/AVIF unchanged; `Accept: image/png` forces PNG output. Unsupported format query values still return 400. `/api/asset/map/<territory>/<index>` remains a composition endpoint and does not use this negotiation; it defaults to **JPEG**. Request an individual map source through `/api/asset?path=...` for raw bytes. The legacy `/i/` route does not use `format` and always returns the stored image format, even if a `format` query parameter or `Accept` header requests something else.

Asset routes accept `version=latest` (default) or an ixion game version with its own published `assets.bin`. Unlike EXD routes, this parameter is not ignored. Asset versions use game-version directory names, not GitHub release titles or upstream's version hashes. Historical versions need their own binary index before they can be served.

For offline testing, set `BM_ASSET_DIRECTORY` to the local `ixion/storage/ui/sdo` directory and `BM_ASSET_PREFIX` to an empty string; no MinIO credentials are required. The independent `bm_asset_index` crate owns IXAS v1 parsing, SqPack directory/filename CRC32 lookup and read-only object access. It does not depend on HTTP, EXD or image conversion. `bm_asset` handles output conversion/map composition with bounded CPU work and an in-memory encoded-response cache.

Following the `bm_data_fs` replacement pattern, this implementation lives in `crates/bm_asset_fs` but retains the Cargo package name `bm_asset`. The server and HTTP crates select it through their dependency paths, so existing `bm_asset::...` imports and `cargo -p bm_asset` commands continue to work. The original upstream source remains in `crates/bm_asset`, with only its package renamed to `bm_asset_bak` relative to the previous local implementation. Keep asset-storage-specific changes in `bm_asset_fs` and `bm_asset_index`, rather than modifying the preserved upstream implementation.

Migration is additive: uploading `assets.bin` does not change `icons.json`, `maps.json`, `current.json` or image objects, so the old Go service can remain live during testing. Validate the new service on a separate port before changing the reverse proxy. Existing caches may retain old responses until their advertised max-age expires.

```powershell
cargo test -p bm_asset_index -p bm_asset -p bm_http
```

For a read-only smoke check against real data, pipe a JSON `bm_asset_index::Config` to `cargo run -p bm_asset --example verify_assets -- <icon-game-path> <map-territory> <map-index>`. It loads the binary index and checks all three encoded output formats, including map composition, plus byte-identical AVIF passthrough when the icon is stored as AVIF. It does not write output files, cache files or MinIO objects. Pass credentials through stdin rather than command-line arguments. For a local check:

```powershell
'{"directory":"D:/Projects/ff14/ixion/storage/ui/sdo","prefix":""}' | cargo run -p bm_asset --example verify_assets -- ui/icon/111000/111008_hr1.tex s1d1 00
```

### Switching supported languages

The merged archive contains data from multiple servers. Set `BM_READ_LANGUAGE_EXCLUDE` to control the exposed languages, for example `[chs,ko]` for global languages, `[ja,en,de,fr,ko]` for Chinese only, or `[]` for all available languages.

Test language support with:

```
/api/sheet/Item/1?fields=Name@lang(chs),Name@lang(de),Name@lang(en),Name@lang(fr),Name@lang(ja)
```

### Release lifecycle tests

Source builds use Rust **1.98.1**, pinned in `rust-toolchain.toml` and the Docker build image (minimum supported Rust: 1.98). x86/x86_64 builds additionally need NASM on `PATH` for rav1d's optimized assembly; the Dockerfile installs it. AArch64 uses the decoder's Rust implementation. Existing OpenSSL/git/SQLite build requirements are unchanged.

```powershell
$env:BM_TEST_ARCHIVE = 'C:\path\to\merged-2026.02.20.0000.0000.zip'
cargo test -p bm_data --lib -- --include-ignored
cargo test -p bm_http --lib
```

The fixture test uses a local HTTP server and temporary directories to exercise download, activation, duplicate triggers, failures, retention and offline restarts.
