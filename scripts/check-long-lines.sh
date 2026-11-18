#!/usr/bin/env bash
# Check core Rust sources by default; --desktop selects the three desktop workspaces.
# Explicit directory arguments select a custom scope.
# Comments and standalone string literals (including concat! segments) are exempt.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
cd "$root"
python3 - "$@" <<'PYTHON'
from pathlib import Path
import re
import sys

CORE_ROOTS = [
    'crates/media-io', 'crates/engine', 'crates/output-cpal', 'crates/storage',
    'crates/test-support', 'crates/library', 'crates/core', 'crates/fs-desktop', 'crates/agent-mcp',
    'apps/cli', 'apps/player-mcp',
]
RAW_STRING = re.compile(r'(?:br|cr|r)(#*)"')
STRING = re.compile(r'(?:b|c)?"')
CHARACTER = re.compile(
    r"(?:b)?'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'"
)


def spans(source):
    """Locate comments and literals, preserving physical lines and nested comments."""
    offset = 0
    while offset < len(source):
        start = offset
        if source.startswith('//', offset):
            end = source.find('\n', offset)
            offset = len(source) if end < 0 else end
            yield start, offset, 'comment'
            continue
        if source.startswith('/*', offset):
            offset += 2
            depth = 1
            while offset < len(source) and depth:
                if source.startswith('/*', offset):
                    depth += 1
                    offset += 2
                elif source.startswith('*/', offset):
                    depth -= 1
                    offset += 2
                else:
                    offset += 1
            yield start, offset, 'comment'
            continue
        boundary = offset == 0 or not (
            source[offset - 1].isalnum() or source[offset - 1] == '_'
        )
        raw = RAW_STRING.match(source, offset) if boundary else None
        if raw:
            closing = '"' + raw[1]
            offset = source.index(closing, offset + len(raw[0])) + len(closing)
            yield start, offset, 'string'
            continue
        string = STRING.match(source, offset)
        if string:
            offset += len(string[0])
            while offset < len(source):
                if source[offset] == '\\':
                    offset += 2
                elif source[offset] == '"':
                    offset += 1
                    break
                else:
                    offset += 1
            yield start, offset, 'string'
            continue
        character = CHARACTER.match(source, offset)
        if character:
            offset += len(character[0])
            yield start, offset, 'character'
            continue
        offset += 1


def failures(path):
    source = path.read_text(encoding='utf-8')
    code = list(source)
    uncommented = list(source)
    for start, end, kind in spans(source):
        for offset in range(start, end):
            if code[offset] != '\n':
                code[offset] = ' '
                if kind == 'comment':
                    uncommented[offset] = ' '
        if kind != 'comment':
            # Mark every physical line covered by a literal, including raw SQL bodies.
            for part in re.finditer(r'[^\n]+', source[start:end]):
                code[start + part.start()] = 'S'
    code_lines = ''.join(code).splitlines()
    uncommented_lines = ''.join(uncommented).splitlines()
    for number, line in enumerate(source.splitlines(), 1):
        if len(line) <= 100:
            continue
        masked = code_lines[number - 1]
        if not masked.strip() or re.fullmatch(r'\s*S\s*[,;]?\s*', masked):
            continue
        # A trailing comment does not increase the width of the code preceding it.
        if len(uncommented_lines[number - 1].rstrip()) > 100:
            yield number, len(line)


# The standalone desktop workspace and unshipped crates are outside the core export.
args = sys.argv[1:]
roots = (['crates/desktop-integration', 'apps/desktop'] if args == ['--desktop']
         else args or CORE_ROOTS)
paths = sorted({
    path for root in roots for path in Path(root).rglob('*.rs')
    if not any(part in ('target', 'node_modules') for part in path.parts)
})
violations = [(path, line, width) for path in paths for line, width in failures(path)]
for path, line, width in violations:
    print(f'{path}:{line}: {width} columns (maximum 100)')
print(f'{len(violations)} long code lines in {len(paths)} Rust files')
sys.exit(bool(violations))
PYTHON
