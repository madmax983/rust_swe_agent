import os

filepath = "src/env/docker.rs"
with open(filepath, "r") as f:
    content = f.read()

content = content.replace(
    """    #[test]
    #[allow(clippy::unwrap_used)]
    #[test]
    fn build_run_args_include_network_none_when_mode_is_none()""",
    """    #[test]
    #[allow(clippy::unwrap_used)]
    fn build_run_args_include_network_none_when_mode_is_none()"""
)

content = content.replace(
    """    #[test]
    #[allow(clippy::unwrap_used)]
    #[test]
    fn build_run_args_network_none_positioned_before_image()""",
    """    #[test]
    #[allow(clippy::unwrap_used)]
    fn build_run_args_network_none_positioned_before_image()"""
)

with open(filepath, "w") as f:
    f.write(content)
