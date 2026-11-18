#!/usr/bin/env bash
# Regenerate encoded audio fixtures in crates/engine/tests/fixtures/.
# usage: FFMPEG=/path/to/ffmpeg scripts/gen-fixtures.sh
set -euo pipefail

FFMPEG="${FFMPEG:-ffmpeg}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/crates/engine/tests/fixtures"
GEN="$ROOT/scripts/gen_signal.py"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$OUT"

FF=("$FFMPEG" -hide_banner -loglevel error -y -bitexact)
META=(-map_metadata -1)

T44=132300; B1=37123; B2=81457
T48=48000

gen() { python3 -I "$GEN" "$@"; }   # out rate total start end

gen "$TMP/sine44_full.wav" 44100 $T44 0 $T44
gen "$TMP/sine44_a.wav"    44100 $T44 0 $B1
gen "$TMP/sine44_b.wav"    44100 $T44 $B1 $B2
gen "$TMP/sine44_c.wav"    44100 $T44 $B2 $T44
gen "$TMP/sine48.wav"      48000 $T48 0 $T48

enc() { # src dst codec-args...
  local src="$1" dst="$2"; shift 2
  "${FF[@]}" -i "$src" "${META[@]}" "$@" "$OUT/$dst"
}

for p in a b c; do
  s="$TMP/sine44_$p.wav"
  enc "$s" "sine44_$p.flac" -c:a flac -sample_fmt s16
  enc "$s" "sine44_$p.wav"  -c:a pcm_s16le -write_bext 0
  enc "$s" "sine44_$p.mp3"  -c:a libmp3lame -b:a 192k -write_xing 1
  enc "$s" "sine44_$p.ogg"  -c:a libvorbis -q:a 5
  enc "$s" "sine44_$p.m4a"  -c:a aac -b:a 192k
  enc "$s" "sine44_$p.opus" -c:a libopus -b:a 128k
done

f="$TMP/sine44_full.wav"
enc "$f" sine44_full.flac -c:a flac -sample_fmt s16
enc "$f" sine44_full.mp3  -c:a libmp3lame -b:a 192k -write_xing 1
enc "$f" sine44_full.ogg  -c:a libvorbis -q:a 5
enc "$f" sine44_full.m4a  -c:a aac -b:a 192k

enc "$TMP/sine48.wav" sine48.flac -c:a flac -sample_fmt s16
enc "$TMP/sine48.wav" sine48.mp3  -c:a libmp3lame -b:a 192k -write_xing 1

"$FFMPEG" -version | head -1 > "$OUT/FFMPEG_VERSION.txt"
ls -l "$OUT"
