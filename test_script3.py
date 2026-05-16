import os
import re

def main():
    for root, dirs, files in os.walk('src'):
        for file in files:
            if file.endswith('.rs'):
                path = os.path.join(root, file)
                with open(path, 'r') as f:
                    content = f.read()
                matches = re.finditer(r'#\[allow\(clippy::too_many_lines\)\]\s*(pub )?(async )?fn ([a-zA-Z0-9_]+)\(', content)
                for match in matches:
                    print(f"{path}: Function: {match.group(3)}")

if __name__ == '__main__':
    main()
