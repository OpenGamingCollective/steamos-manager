#!/bin/sh
set -e
zbus-xmlgen file ../../data/interfaces/com.steampowered.SteamOSManager1.xml

for f in *.rs; do
    if [ "$f" = "lib.rs" ]; then
        continue
    fi

    if [ -s "patches/$f.patch" ]; then
        patch "$f" -p0 < patches/$f.patch
        continue
    fi

    patch "$f" -p0 < patches/fix.patch
done
