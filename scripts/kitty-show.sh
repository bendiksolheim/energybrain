#!/usr/bin/env bash
# Shows PNG files in the terminal with the kitty graphics protocol (Rex, kitty, Ghostty, …),
# each scaled to the terminal's width. Usage: scripts/kitty-show.sh FILE.png…
set -euo pipefail

cols=$(tput cols 2>/dev/null || echo 120)
for file in "$@"; do
  printf '%s\n' "$(basename "$file")"
  first=1
  # One 4096-byte base64 chunk per line; m=1 on every chunk except the last.
  mapfile -t chunks < <(base64 -w 4096 "$file")
  last=$((${#chunks[@]} - 1))
  for i in "${!chunks[@]}"; do
    more=$([ "$i" -lt "$last" ] && echo 1 || echo 0)
    if [ "$first" = 1 ]; then
      printf '\033_Ga=T,f=100,q=2,c=%s,m=%s;%s\033\\' "$cols" "$more" "${chunks[$i]}"
      first=0
    else
      printf '\033_Gm=%s;%s\033\\' "$more" "${chunks[$i]}"
    fi
  done
  printf '\n\n'
done
