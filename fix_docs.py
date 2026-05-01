import re

def insert_doc(lines, pattern, doc):
    for i, line in enumerate(lines):
        if re.search(pattern, line):
            # Check if there is already a doc comment
            if i > 0 and lines[i-1].strip().startswith("///"):
                continue
            # Also check if it's not a module definition but a struct/enum/etc.
            # Insert before the line, maintaining indentation
            indent = len(line) - len(line.lstrip())
            doc_lines = [(" " * indent) + "/// " + d + "\n" for d in doc.split("\n")]

            # Since some things have decorators like #[derive...], we might need to find the correct insertion point.
            # Usually we put doc comments before decorators.
            insert_idx = i
            while insert_idx > 0 and lines[insert_idx-1].strip().startswith("#["):
                insert_idx -= 1

            lines[insert_idx:insert_idx] = doc_lines
            return True
    return False

# I'll manually create patch files or just modify with regex via python.
