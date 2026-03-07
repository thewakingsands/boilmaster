# Boilmaster (ffcafe fork)

Web service for Final Fantasy XIV game data and asset discovery, forked from [ackwell](https://github.com/ackwell/boilmaster) by FFCafe.

## Notes for API Users

We maintain an instance of Boilmaster at <https://xivapi-v2.xivcdn.com/> and provide Chinese-accelerated access.

You can use it to serve Chinese users, but note the following differences:

* We provide game data in the following languages: Chinese, Japanese, English, German, and French.
* All version-related functionalities are disabled. The `version` parameter in the URL will not be handled.
* No asset retrieval features are available. API endpoints starting with `/api/asset` will return 404 due to the high cost of hosting assets.

## Installation

### Docker Usage

Boilmaster is published as a Docker image on the GitHub Container Registry. An example `docker-compose.yml` file like the one below can be used to bring the service online.

```yml
services:
  boilmaster:
    image: ghcr.io/thewakingsands/boilmaster:latest
    container_name: boilmaster
    environment:
      # Other configuration here, see the Configuration section below for more information.
    volumes:
      - ${PWD}/persist:/app/persist
      - /path/to/your/game:/app/game:ro
    ports:
      - 8080:8080
    restart: unless-stopped
```

### Exd files

We use external data files instead of tracking all patches of the game. The following files should be present in the `game` directory
(or mounted to `/app/game` when using Docker):

* ffxivgame.ver
* sqpack/ffxiv/0a0000.win32.dat
* sqpack/ffxiv/0a0000.win32.index
* sqpack/ffxiv/0a0000.win32.index2

### Switching supported languages

Multi-language queries can be achieved with the `exd build` command of [ixion](https://github.com/thewakingsands/ixion), which generates a merged sqpack file from different servers.
Set the environment variable `BM_READ_LANGUAGE_EXCLUDE` for different setups. For example:

* Global: `[chs,ko]`
* SDO: `[ja,en,de,fr,ko]`
* Actoz: `[ja,en,de,fr,chs]`
* Combination of all available languages: `[]`

Test language support with the following path:

```
/api/1/sheet/Item/1?fields=Name@lang(chs),Name@lang(de),Name@lang(en),Name@lang(fr),Name@lang(ja)
```
