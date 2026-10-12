#!/bin/sh
set -eu

data_dir=/var/lib/mcpmem
if [ ! -d "$data_dir" ] && [ -w "$(dirname "$data_dir")" ]; then
    mkdir -p "$data_dir"
fi

exec mcpmem --config /etc/mcpmem/mcpmem.toml "$@"
