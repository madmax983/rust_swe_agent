with open("src/agent/default.rs", "r") as f:
    content = f.read()

lines = content.split('\n')
for i, line in enumerate(lines):
    if "async fn step(&mut self)" in line:
        start_idx = i
        break

end_idx = start_idx + 100
for i in range(start_idx, end_idx):
    if "self.maybe_warn_wallclock_deadline();" in lines[i]:
        end_idx = i + 1
        break

print('\n'.join(lines[start_idx:end_idx]))
