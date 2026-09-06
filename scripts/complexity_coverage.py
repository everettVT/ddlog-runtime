#!/usr/bin/env python3
"""Diagnostic CRAP estimates from Lizard Rust complexity and LLVM LCOV lines.

Requires lizard==1.24.0. Run from the repository root, passing an LCOV file.
This uses executable-line coverage, not branch coverage. No score is invented
for functions without coverage mapping. Generated native sources are excluded.
"""
import json
from pathlib import Path
import sys

import lizard


def report(path):
    coverage = {}
    current = None
    for line in Path(path).read_text().splitlines():
        if line.startswith("SF:"):
            current = str(Path(line[3:]).resolve())
            coverage.setdefault(current, {})
        elif line.startswith("DA:") and current is not None:
            number, hits, *_ = line[3:].split(",")
            number, hits = int(number), int(hits)
            coverage[current][number] = max(hits, coverage[current].get(number, 0))
    rows = []
    for source in sorted(Path("src").glob("*.rs")):
        lines = coverage.get(str(source.resolve()), {})
        for function in lizard.analyze_file(str(source)).function_list:
            hits = [count for number, count in lines.items()
                    if function.start_line <= number <= function.end_line]
            fraction = sum(count > 0 for count in hits) / len(hits) if hits else None
            complexity = function.cyclomatic_complexity
            rows.append({
                "file": str(source), "function": function.name,
                "line": function.start_line, "complexity": complexity,
                "line_coverage": fraction,
                "crap_estimate": (complexity ** 2 * (1 - fraction) ** 3 + complexity
                                  if fraction is not None else None),
            })
    return sorted(rows, key=lambda row: row["crap_estimate"] or 0, reverse=True)


if __name__ == "__main__":
    print(json.dumps(report(sys.argv[1]), indent=2))
