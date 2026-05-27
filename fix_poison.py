import re

with open('src/stream/event_log.rs', 'r') as f:
    content = f.read()

content = re.sub(
    r"match self\.writer\.lock\(\) \{",
    "match self.writer.lock().or_else(|e| Ok::<_, ()>(e.into_inner())) {",
    content,
    count=1
)

content = re.sub(
    r"if let Ok\(mut w\) = self\.writer\.lock\(\) \{",
    "if let Ok(mut w) = self.writer.lock().or_else(|e| Ok::<_, ()>(e.into_inner())) {",
    content,
    count=1
)

content = re.sub(
    r"Err\(_\) => \{",
    "Err(()) => {",
    content,
    count=1
)


with open('src/stream/event_log.rs', 'w') as f:
    f.write(content)
