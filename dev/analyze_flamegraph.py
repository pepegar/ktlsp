#!/usr/bin/env python3

"""Print aggregate self-sample hotspots from inferno/pprof flamegraph SVGs."""

import argparse
import bisect
import html
import re
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path


FRAME_RE = re.compile(
    r'<g><title>(.*?) \([\d,]+ samples?, [\d.]+%\)</title>'
    r'<rect[^>]*\by="(\d+)"[^>]*\bfg:x="(\d+)" fg:w="(\d+)"'
)
TOTAL_RE = re.compile(r'total_samples="(\d+)"')


@dataclass(frozen=True)
class Frame:
    name: str
    y: int
    x: int
    width: int


def hotspots(path: Path) -> tuple[int, list[tuple[int, str]]]:
    source = path.read_text()
    total_match = TOTAL_RE.search(source)
    if total_match is None:
        raise ValueError(f"{path}: missing total_samples")
    total = int(total_match.group(1))
    frames = [
        Frame(html.unescape(name), int(y), int(x), int(width))
        for name, y, x, width in FRAME_RE.findall(source)
    ]
    by_y: dict[int, list[Frame]] = defaultdict(list)
    for frame in frames:
        by_y[frame.y].append(frame)
    for level in by_y.values():
        level.sort(key=lambda frame: frame.x)
    starts_by_y = {
        y: [frame.x for frame in level]
        for y, level in by_y.items()
    }

    self_samples: dict[str, int] = defaultdict(int)
    for frame in frames:
        children = by_y.get(frame.y - 16, [])
        starts = starts_by_y.get(frame.y - 16, [])
        index = bisect.bisect_left(starts, frame.x)
        child_width = 0
        end = frame.x + frame.width
        while index < len(children) and children[index].x < end:
            child = children[index]
            if child.x + child.width <= end:
                child_width += child.width
            index += 1
        own = frame.width - child_width
        if own > 0 and frame.name != "all":
            self_samples[frame.name] += own

    return total, sorted(
        ((samples, name) for name, samples in self_samples.items()),
        reverse=True,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("svg", nargs="+", type=Path)
    parser.add_argument("--limit", type=int, default=20)
    args = parser.parse_args()

    for path in args.svg:
        total, rows = hotspots(path)
        print(f"{path} ({total:,} samples)")
        for samples, name in rows[: args.limit]:
            print(f"  {samples:>7,}  {samples / total:>7.2%}  {name}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
