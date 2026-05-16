import sys

def main():
    with open('src/cli/mod.rs', 'r') as f:
        content = f.read()

    import re
    matches = re.finditer(r'#\[allow\(clippy::too_many_lines\)\]\s*(pub )?(async )?fn ([a-zA-Z0-9_]+)\(', content)

    for match in matches:
        print(f"Function: {match.group(3)}")

if __name__ == '__main__':
    main()
