import sys

with open('src/trajectory/export.rs', 'r') as f:
    content = f.read()

# Add #[allow(clippy::expect_used)] to the test
content = content.replace('#[test]\n    fn test_jupyter_export_format()', '#[allow(clippy::expect_used)]\n    #[test]\n    fn test_jupyter_export_format()')

with open('src/trajectory/export.rs', 'w') as f:
    f.write(content)
