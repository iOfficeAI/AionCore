#!/usr/bin/env bash
set -euo pipefail

config_file="${AIONUI_CONFIG_DEV_FILE:-$HOME/.aionui-config-dev/aionui-config.txt}"
if [[ ! -f "$config_file" ]]; then
    echo "config file not found: $config_file" >&2
    exit 1
fi

if base64 --help 2>&1 | grep -q -- "-D"; then
    decoded=$(base64 -D -i "$config_file")
else
    decoded=$(base64 -d "$config_file")
fi

plain=$(printf "%s" "$decoded" | python3 -c 'import sys, urllib.parse; print(urllib.parse.unquote(sys.stdin.read()))')

if command -v pbcopy >/dev/null 2>&1; then
    printf "%s" "$plain" | pbcopy
    echo "Config copied to clipboard"
elif command -v wl-copy >/dev/null 2>&1; then
    printf "%s" "$plain" | wl-copy
    echo "Config copied to clipboard"
elif command -v xclip >/dev/null 2>&1; then
    printf "%s" "$plain" | xclip -selection clipboard
    echo "Config copied to clipboard"
else
    printf "%s\n" "$plain"
fi
