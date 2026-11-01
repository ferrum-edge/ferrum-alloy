"""Share of each formatter profile's instructions per bucket, as in docs/benchmarks.md.

usage: python3 -I buckets.py DIRECTORY   (holding the gunzipped *-annotate-*.stdout files)

Each annotation line's self instructions go to the first bucket whose
pattern matches its file:function name; totals sum PROGRAM TOTALS over
all threads.
"""

import collections
import glob
import re
import sys

BUCKETS = [
    ("serde_json parsing", r"serde_json-[0-9.]+/src/(read|de)\.rs"),
    ("serde_json serialization", r"serde_json-[0-9.]+/src/ser\.rs"),
    ("tracing-subscriber fmt", r"tracing-subscriber-[0-9.]+/src/fmt|tracing_subscriber::fmt"),
    ("UTF-8 validation", r"core::str::converts::from_utf8"),
    ("ferrum_alloy_telemetry::json", r"ferrum_alloy_telemetry::json|ferrum-alloy-telemetry/src/json"),
    ("malloc/free", r"malloc\.c"),
    ("memcpy/memmove/memcmp/memset", r"multiarch/mem"),
]

for scenario in ("alloy-logs-fmt", "alloy-logs"):
    total, shares = 0, collections.Counter()
    for path in glob.glob(f"{sys.argv[1]}/profile-{scenario}-annotate-*.stdout"):
        for line in open(path):
            match = re.match(r"\s*([0-9,]+)\s+(.*)", line)
            if not match:
                continue
            value, name = int(match[1].replace(",", "")), match[2]
            if "PROGRAM TOTALS" in name:
                total += value
            elif "file:function" not in name:
                for bucket, pattern in BUCKETS:
                    if re.search(pattern, name):
                        shares[bucket] += value
                        break
    print(f"{scenario}: {total:,} instructions")
    for bucket, _ in BUCKETS:
        print(f"  {bucket:30} {shares[bucket] / total * 100:5.1f}%")
