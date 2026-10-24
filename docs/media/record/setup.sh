#!/usr/bin/env bash
# Демо-каталог для записи: временный HOME ($1) с ~/demo.
#
#   ~/demo/backup     пусто — сюда копируют в ops
#   ~/demo/downloads  project.zip (настоящий zip), ubuntu.iso, video.mp4
#   ~/demo/logs       app.log на 400 строк
#   ~/demo/photos     IMG_0001..3.jpg
#   ~/demo/project    git-репозиторий: чистые файлы в корне, изменённые docs/ и src/
#
# Содержимое детерминировано, кроме случайных байтов «бинарных» файлов.
# Даты выставляются в конце: любая запись в директорию сдвигает её mtime.
set -euo pipefail

home="$1"
root="$(cd "$(dirname "$0")/../../.." && pwd)"
# Исходники janus на момент первой записи демо: размеры файлов видны в GIF.
rev=3d5f799
demo="$home/demo"

rm -rf "$home"
mkdir -p "$demo"/{backup,downloads,logs,photos,project/docs,project/src}

# Файл из случайного начала и разреженного хвоста: в просмотре — шестнадцатеричный
# дамп, а места на диске почти не занимает.
binary() {
    head -c 65536 /dev/urandom > "$1"
    truncate -s "$2" "$1"
}

binary "$demo/photos/IMG_0001.jpg" 5242880
binary "$demo/photos/IMG_0002.jpg" 6291456
binary "$demo/photos/IMG_0003.jpg" 7340032
binary "$demo/downloads/ubuntu.iso" 314572800
binary "$demo/downloads/video.mp4" 125829120

# project.zip — часть исходников, чтобы архив открывался как директория.
stage="$(mktemp -d)"
mkdir -p "$stage/src"
git -C "$root" show "$rev:Cargo.toml" | sed 's/^name = "janus"/name = "fmgr"/' > "$stage/Cargo.toml"
for f in listing shell git panel; do
    git -C "$root" show "$rev:src/$f.rs" > "$stage/src/$f.rs"
done
(cd "$stage" && zip -q -X project.zip src/ src/listing.rs src/shell.rs src/git.rs src/panel.rs Cargo.toml)
mv "$stage/project.zip" "$demo/downloads/"

# Журнал: поиск по содержимому ищет в нём «retrying».
python3 - "$demo/logs/app.log" <<'EOF'
import random, sys
rnd = random.Random(14)
levels = ["DEBUG", "INFO ", "WARN ", "ERROR"]
templates = [
    lambda: f"request served GET /api/users 200 in {rnd.randint(3, 250)}ms",
    lambda: f"retrying upstream https://api.example.com/v1/items ({rnd.randint(1, 3)} of 3)",
    lambda: f'cache miss for key "user:{rnd.randint(1, 250)}"',
    lambda: f"connection from 192.168.1.{rnd.randint(2, 254)} accepted",
    lambda: f"worker {rnd.randint(1, 250)} finished batch",
]
t = 9 * 3600
with open(sys.argv[1], "w") as out:
    for _ in range(400):
        t += rnd.randint(1, 40)
        stamp = f"{t // 3600:02}:{t // 60 % 60:02}:{t % 60:02}"
        out.write(f"2026-10-14 {stamp} {rnd.choice(levels)} {rnd.choice(templates)()}\n")
EOF

# Проект.
project="$demo/project"
cd "$project"
printf 'target/\nnode_modules/\n*.log\n' > .gitignore
printf '#!/bin/sh\ncargo build --release\n' > build.sh
chmod +x build.sh
git -C "$root" show "$rev:Cargo.toml" | sed 's/^name = "janus"/name = "fmgr"/' > Cargo.toml
cat > config.toml <<'EOF'
[server]
host = "0.0.0.0"
port = 8080
debug = false

[database]
url = "postgres://localhost/app"
pool = 16
EOF
cat > README.md <<'EOF'
# fmgr

Two-panel terminal file manager.

    cargo run --release
EOF
cat > package.json <<'EOF'
{
  "name": "fmgr-docs",
  "version": "0.1.0",
  "private": true,
  "scripts": {
    "build": "vitepress build docs",
    "dev": "vitepress dev docs"
  }
}
EOF
{
    echo '.PHONY: build test lint release clean install'
    echo
    for target in build test lint release clean install; do
        echo "# $target: см. README."
        echo "$target:"
        for i in 1 2 3 4 5 6 7 8; do
            echo "	@cargo $target --quiet --message-format short  # step $i"
        done
        echo
    done
} > Makefile
printf '# Architecture\n\nTwo panels, one event loop.\n' > docs/architecture.md
for f in config editor git highlight i18n listing main ops panel shell; do
    git -C "$root" show "$rev:src/$f.rs" > "src/$f.rs"
done

git init -q -b master
git add -A
git -c user.name=demo -c user.email=demo@example.com commit -q -m "Initial import"
# Незакоммиченные изменения: docs/ и src/ подсвечиваются как изменённые.
printf '\n// TODO: progress for large files\n' >> src/ops.rs
printf '# Notes\n\nKeys and panels: see README.\n' > docs/notes.md
git status --porcelain > /dev/null

# Даты.
cd "$demo"
find "$demo" -exec touch -h -t 202610141210 {} +
touch -t 202610141216 project/.gitignore project/Makefile project/package.json \
    project/docs project/docs/* \
    project/src/{config,editor,highlight,i18n,ops,panel}.rs
touch -t 202610141255 project/src
touch -t 202610141417 project/.git
touch -t 202610141420 project
touch -t 202610141317 downloads
touch -t 202610141430 backup
touch -t 202610141211 "$demo"
