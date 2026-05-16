import sys

def main():
    with open('src/agent/default.rs', 'r') as f:
        content = f.read()

    # Search for functions over 100 lines
    import re

    # We will try to find functions that have #[allow(clippy::too_many_lines)]
    matches = re.finditer(r'#\[allow\(clippy::too_many_lines\)\]\s*(pub )?(async )?fn ([a-zA-Z0-9_]+)\(', content)

    for match in matches:
        print(f"Function: {match.group(3)}")

if __name__ == '__main__':
    main()
