import os
import re

def analyze_file(filepath):
    with open(filepath, 'r') as f:
        lines = f.readlines()

    in_fn = False
    fn_name = ""
    fn_start = 0
    nesting = 0
    max_nesting = 0

    results = []

    for i, line in enumerate(lines):
        line = line.split('//')[0] # remove comments
        if not in_fn:
            match = re.search(r'\s*fn\s+([a-zA-Z0-9_]+)\s*\(', line)
            if match:
                in_fn = True
                fn_name = match.group(1)
                fn_start = i
                nesting = line.count('{') - line.count('}')
                max_nesting = nesting
        else:
            nesting += line.count('{') - line.count('}')
            if nesting > max_nesting:
                max_nesting = nesting
            if nesting == 0:
                in_fn = False
                length = i - fn_start
                if length > 50 or max_nesting > 4:
                    results.append((filepath, fn_name, length, max_nesting, fn_start + 1))

    return results

all_results = []
for root, _, files in os.walk('src'):
    for file in files:
        if file.endswith('.rs'):
            filepath = os.path.join(root, file)
            all_results.extend(analyze_file(filepath))

all_results.sort(key=lambda x: x[2], reverse=True)
for r in all_results[:20]:
    print(f"{r[0]}:{r[4]} - fn {r[1]} - {r[2]} lines - max depth {r[3]}")
