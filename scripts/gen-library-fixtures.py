#!/usr/bin/env python3
"""Generate a small music library for the `library` crate's integration tests.

usage: FFMPEG=/path/to/ffmpeg python3 scripts/gen-library-fixtures.py OUT_DIR

Copyright (c) 2026 CocytusChen; MIT OR Apache-2.0. All music metadata below is
invented for encoding tests; the tones and plain-color cover are original synthetic
fixtures. FFmpeg is an external tool and is not distributed with the repository.

Stdlib only. Audio comes from ffmpeg (0.3 s tones); ffmpeg also writes the ordinary tags.
Legacy-encoded ID3 tags (GBK, Big5, Shift_JIS bytes in frames marked Latin-1, as old
Chinese/Japanese taggers wrote them) are written by this script, because ffmpeg only writes
UTF-8/UTF-16. OUT_DIR is created; existing files in it are overwritten. The output is not
committed: tests regenerate it into the cargo target directory.

Layout (see crates/library/tests/scan.rs for what each case checks):
  Artist A/Album One (2001)/      FLAC, track numbers, genre, ReplayGain, embedded cover on
                                  track 1, cover.jpg next to all
  Artist B/Double Album/CD1, CD2/ MP3 (ID3v2.4), two discs, album artist
  Compilations/Now Hits/          Ogg Vorbis, ALBUMARTIST=Various Artists, COMPILATION=1,
                                  folder cover Cover.PNG (no embedded art)
  Compilations/Summer Mix/        M4A (AAC), compilation flag, no album artist
  Untagged/deep/nested/folder/    WAV and FLAC without any tags
  CJK/...                         Chinese, Japanese and full-width titles (UTF-8 tags)
  Tagged WAV/                     WAV with RIFF INFO tags
  Legacy/...                      ID3v2.3 / ID3v1 with GBK, Big5, Shift_JIS and real Latin-1,
                                  and UTF-16 frames that must not be "repaired"
  Broken/                         a file with an audio extension that is not audio
  plus non-audio files (cover.jpg, notes.txt) that must be ignored
"""

import os
import shutil
import struct
import subprocess
import sys
import tempfile

FFMPEG = os.environ.get("FFMPEG", "ffmpeg")
DURATION = "0.3"


def ffmpeg(*args):
    cmd = [FFMPEG, "-hide_banner", "-loglevel", "error", "-y", *args]
    subprocess.run(cmd, check=True)


def tone(freq):
    return ["-f", "lavfi", "-i", f"sine=frequency={freq}:duration={DURATION}:sample_rate=44100"]


def metadata_args(tags):
    out = []
    for key, value in tags.items():
        out += ["-metadata", f"{key}={value}"]
    return out


CODECS = {
    ".flac": ["-c:a", "flac", "-sample_fmt", "s16"],
    ".mp3": ["-c:a", "libmp3lame", "-b:a", "128k"],
    ".ogg": ["-c:a", "libvorbis", "-q:a", "3"],
    ".m4a": ["-c:a", "aac", "-b:a", "96k"],
    ".wav": ["-c:a", "pcm_s16le"],
}


def encode(path, tags=None, freq=440, cover=None, extra=()):
    """Encodes a stereo tone to `path` (format from the extension) with `tags`."""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    ext = os.path.splitext(path)[1].lower()
    args = tone(freq)
    if cover:
        args += ["-i", cover, "-map", "0:a", "-map", "1:v", "-c:v", "copy",
                 "-disposition:v", "attached_pic"]
    args += ["-ac", "2", *CODECS[ext], "-map_metadata", "-1", "-bitexact", *extra]
    args += metadata_args(tags or {})
    ffmpeg(*args, path)


# ---- hand-written legacy ID3 tags ----

def syncsafe(n):
    return bytes([(n >> 21) & 0x7F, (n >> 14) & 0x7F, (n >> 7) & 0x7F, n & 0x7F])


def id3v23(frames, encoding):
    """An ID3v2.3 tag whose text frames claim Latin-1 (encoding byte 0) but hold `encoding`;
    with encoding "utf-16", proper UTF-16 frames (encoding byte 1, with BOM)."""
    body = b""
    for frame_id, text in frames:
        marker = b"\x01" if encoding == "utf-16" else b"\x00"
        payload = marker + text.encode(encoding)
        body += frame_id.encode("ascii") + struct.pack(">I", len(payload)) + b"\x00\x00" + payload
    return b"ID3\x03\x00\x00" + syncsafe(len(body)) + body


def id3v1(title, artist, album, year, track, encoding):
    """An ID3v1.1 tag with text in `encoding` (ID3v1 is nominally Latin-1)."""
    def field(text, size):
        raw = text.encode(encoding)
        assert len(raw) <= size, (text, encoding)
        return raw.ljust(size, b"\x00")
    return (b"TAG" + field(title, 30) + field(artist, 30) + field(album, 30)
            + field(year, 4) + b"\x00" * 28 + b"\x00" + bytes([track]) + b"\xff")


def untagged_mp3(path, freq):
    encode(path, freq=freq, extra=["-id3v2_version", "0", "-write_id3v1", "0"])


def legacy_v23(path, encoding, title, artist, album, track, freq):
    untagged_mp3(path, freq)
    with open(path, "rb") as f:
        audio = f.read()
    frames = [("TIT2", title), ("TPE1", artist), ("TALB", album), ("TRCK", str(track))]
    with open(path, "wb") as f:
        f.write(id3v23(frames, encoding) + audio)


def legacy_v1(path, encoding, title, artist, album, year, track, freq):
    untagged_mp3(path, freq)
    with open(path, "ab") as f:
        f.write(id3v1(title, artist, album, year, track, encoding))


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    p = lambda *parts: os.path.join(out, *parts)

    with tempfile.TemporaryDirectory() as tmp:
        cover = os.path.join(tmp, "cover.png")
        ffmpeg("-f", "lavfi", "-i", "color=c=red:s=16x16", "-frames:v", "1", cover)

        # Album with numbering, genre, ReplayGain and an embedded cover on track 1.
        for n, title in enumerate(["First Light", "Second Wind", "Third Eye"], 1):
            encode(p("Artist A", "Album One (2001)", f"0{n} {title}.flac"), {
                "title": title, "artist": "Artist A", "album": "Album One",
                "date": "2001-05-01", "track": f"{n}/3", "genre": "Rock",
                "REPLAYGAIN_TRACK_GAIN": "-6.50 dB", "REPLAYGAIN_TRACK_PEAK": "0.988",
                # A mapped extra tag and one lofty does not know.
                **({"COMPOSER": "Eran Savelle", "MY_CUSTOM": "Custom Value"} if n == 1 else {}),
            }, freq=300 + n * 10, cover=cover if n == 1 else None)

        # Two discs.
        for disc in (1, 2):
            for n in (1, 2):
                encode(p("Artist B", "Double Album", f"CD{disc}", f"0{n}.mp3"), {
                    "title": f"Disc {disc} Track {n}", "artist": "Artist B",
                    "album_artist": "Artist B", "album": "Double Album",
                    "track": f"{n}/2", "disc": f"{disc}/2", "date": "1999",
                    # TCOM and a TXXX frame.
                    **({"composer": "Elvio Marnovel", "CATALOG_CUSTOM": "XYZ-1"}
                       if (disc, n) == (1, 1) else {}),
                }, freq=400 + disc * 20 + n)

        # Compilation with an explicit album artist.
        for n, artist in enumerate(["Singer One", "Singer Two", "Band Three"], 1):
            encode(p("Compilations", "Now Hits", f"0{n}.ogg"), {
                "TITLE": f"Hit {n}", "ARTIST": artist, "ALBUM": "Now Hits",
                "ALBUMARTIST": "Various Artists", "COMPILATION": "1", "TRACKNUMBER": str(n),
            }, freq=500 + n)
        shutil.copyfile(cover, p("Compilations", "Now Hits", "Cover.PNG"))

        # Compilation flag only (MP4 cpil), different artists, no album artist.
        for n, artist in enumerate(["DJ Alpha", "DJ Beta", "DJ Gamma"], 1):
            encode(p("Compilations", "Summer Mix", f"0{n}.m4a"), {
                "title": f"Summer {n}", "artist": artist, "album": "Summer Mix",
                "compilation": "1", "track": str(n),
            }, freq=600 + n)

        # No tags at all, several folder levels deep.
        encode(p("Untagged", "deep", "nested", "folder", "noise.wav"), freq=700)
        encode(p("Untagged", "no tags.flac"), freq=710)

        # Unicode tags.
        encode(p("CJK", "周砚舟", "叶窗集", "01 云盒之声.flac"), {
            "title": "云盒之声", "artist": "周砚舟", "album": "叶窗集", "track": "1", "date": "2003",
        }, freq=800)
        encode(p("CJK", "周砚舟", "叶窗集", "02 晶港.flac"), {
            "title": "晶港", "artist": "周砚舟", "album": "叶窗集", "track": "2", "date": "2003",
        }, freq=810)
        encode(p("CJK", "Fullwidth", "ＡＢＣ　Ｓｏｎｇ.ogg"), {
            "TITLE": "ＡＢＣ　Ｓｏｎｇ", "ARTIST": "ＤＪ　Ｗｉｄｅ",
        }, freq=820)
        encode(p("CJK", "Anime", "静かな雲箱のアーチ.m4a"), {
            "title": "静かな雲箱のアーチ", "artist": "高雲帆子", "album": "空箱紀行アーカイヴス",
        }, freq=830)

        # RIFF INFO tags.
        encode(p("Tagged WAV", "wav song.wav"), {"title": "Wave Title", "artist": "Wave Artist"},
               freq=840)

        # Legacy encodings in Latin-1 frames.
        legacy_v23(p("Legacy", "GBK v2.3", "sanlengwu.mp3"), "gbk",
                   "三棱雾", "周砚舟", "三棱雾", 1, 900)
        legacy_v23(p("Legacy", "Big5 v2.3", "xingqiao.mp3"), "big5",
                   "星橋漫步", "星舟團", "雲盒航圖", 2, 910)
        legacy_v23(p("Legacy", "Latin-1 v2.3", "cafe.mp3"), "latin-1",
                   "Café des Nues", "Vélinéa", "Déjà Nu", 3, 920)
        legacy_v23(p("Legacy", "Latin-1 v2.3", "agaevis.mp3"), "latin-1",
                   "Ágævis nyrven", "Sivar Nól", "Ágævis nyrven", 4, 925)
        # Unicode frames whose text happens to look like GBK mojibake: must stay as written.
        legacy_v23(p("Legacy", "UTF-16 v2.3", "unicode.mp3"), "utf-16",
                   "ÈýÀâÎí", "ÖÜÑâÖÛ", "Unicode Frames", 5, 950)
        legacy_v1(p("Legacy", "Shift_JIS v1", "hoshibako.mp3"), "shift_jis",
                  "星箱桜", "星野ノノ", "ホシバコ曲集", "2011", 4, 930)
        legacy_v1(p("Legacy", "GBK v1", "jingwan.mp3"), "gbk",
                  "晶港的故事", "周砚舟", "叶窗集", "2003", 5, 940)

    # Not audio, or not decodable.
    os.makedirs(p("Broken"), exist_ok=True)
    with open(p("Broken", "garbage.flac"), "wb") as f:
        f.write(b"this is not a flac file" * 10)
    with open(p("Artist A", "Album One (2001)", "cover.jpg"), "wb") as f:
        f.write(b"\xff\xd8\xff\xd9")
    with open(p("notes.txt"), "w") as f:
        f.write("not music\n")


if __name__ == "__main__":
    main()
