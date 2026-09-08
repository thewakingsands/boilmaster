This directory stores downloaded ixion releases, one directory per release title.

Boilmaster downloads the newest stable release at startup and retains at most 10 local releases. Each managed directory contains `release.json`, `ffxivgame.ver` and `sqpack/ffxiv` data extracted from the merged ZIP.

Set `BM_GAME_DIRECTORY` to change this location. It must be writable. Unmanaged files from the former manually mounted installation are not imported or deleted.
