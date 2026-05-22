import re

with open("src/cost.rs", "r") as f:
    content = f.read()

print("Functions in src/cost.rs:")
for match in re.finditer(r'(?:pub\s+)?(?:const\s+)?fn\s+(\w+)\s*\(', content):
    print(match.group(1))
