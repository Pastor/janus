#!/usr/bin/env bash
# Перезаписывает демо-GIF из docs/media/{en,ru} по сценариям VHS.
#
#   docs/media/record/record.sh            # все сценарии
#   docs/media/record/record.sh ops rename # только перечисленные
#
# Нужны vhs (https://github.com/charmbracelet/vhs), zip, python3 и шрифт Menlo.
# Список дисков в drives.gif и корень «/» берутся с машины, где идёт запись.
# Каждый сценарий пишется с чистого демо-каталога: setup.sh создаёт
# временный HOME с ~/demo, а tapes/<имя>.sh, если есть, дополняет его
# (настройки, файлы, нужные только этому сценарию). В сценариях
# подставляются @JANUS@ (собранный бинарник), @LANG@ (en/ru) и @HOME@.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
media="$root/docs/media"

cargo build --release --manifest-path "$root/Cargo.toml"
janus="$root/target/release/janus"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
home="$work/home"

if [ $# -gt 0 ]; then
    names=("$@")
else
    names=()
    for tape in "$here"/tapes/*.tape; do
        names+=("$(basename "$tape" .tape)")
    done
fi

for name in "${names[@]}"; do
    for lang in en ru; do
        echo "== $name ($lang)"
        "$here/setup.sh" "$home"
        if [ -x "$here/tapes/$name.sh" ]; then
            "$here/tapes/$name.sh" "$home" "$lang"
        fi
        tape="$work/$name-$lang.tape"
        {
            echo "Output \"$work/$name.gif\""
            cat "$here/header.tape" "$here/tapes/$name.tape"
        } | sed -e "s#@JANUS@#$janus#g" -e "s#@LANG@#$lang#g" -e "s#@HOME@#$home#g" > "$tape"
        vhs "$tape" > /dev/null
        mv "$work/$name.gif" "$media/$lang/$name.gif"
    done
done
