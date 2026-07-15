import re

with open("src/env/docker.rs", "r") as f:
    content = f.read()

target = """    fn build_run_args_include_network_none_when_mode_is_none() {
        let args = build_run_args("my-image", "/workspace", LABEL, Some("none"));
        let network_pos = args.iter().position(|a| a == "--network");
        assert!(
            network_pos.is_some(),
            "expected --network flag in args: {args:?}"
        );
        assert_eq!(
            args.get(network_pos.unwrap() + 1).map(String::as_str),
            Some("none")
        );
    }"""
replacement = """    fn build_run_args_include_network_none_when_mode_is_none() {
        let args = build_run_args("my-image", "/workspace", LABEL, Some("none"));
        let network_pos = args.iter().position(|a| a == "--network");
        assert!(
            network_pos.is_some(),
            "expected --network flag in args: {args:?}"
        );
        assert_eq!(
            network_pos.and_then(|pos| args.get(pos + 1)).map(String::as_str),
            Some("none")
        );
    }"""
content = content.replace(target, replacement)

target2 = """    #[test]
    fn build_run_args_network_none_positioned_before_image() {
        let args = build_run_args("my-image", "/workspace", LABEL, Some("none"));
        let network_pos = args.iter().position(|a| a == "--network").expect("should have network flag");
        let image_pos = args.iter().position(|a| a == "my-image").expect("should have image");
        assert!(
            network_pos < image_pos,
            "--network must appear before the image name"
        );
    }"""
replacement2 = """    #[test]
    #[allow(clippy::expect_used)]
    fn build_run_args_network_none_positioned_before_image() {
        let args = build_run_args("my-image", "/workspace", LABEL, Some("none"));
        let network_pos = args.iter().position(|a| a == "--network").expect("should have network flag");
        let image_pos = args.iter().position(|a| a == "my-image").expect("should have image");
        assert!(
            network_pos < image_pos,
            "--network must appear before the image name"
        );
    }"""
content = content.replace(target2, replacement2)

target3 = """    #[test]
    fn custom_network_injected_before_image() {
        let mut cfg = DockerEnvironmentConfig::default();
        cfg.network = DockerNetwork::Custom("my-custom-net".to_owned());
        let env = DockerEnvironment::new(&cfg, Path::new("/dummy/workdir"));
        let args = env.build_docker_run_args();

        let network_pos = args.iter().position(|a| a == "--network").expect("should have network flag");
        let image_pos = args.iter().position(|a| a == "my-image").expect("should have image");
        assert!(network_pos < image_pos);
        assert_eq!(args[network_pos + 1], "my-custom-net");
    }"""
replacement3 = """    #[test]
    #[allow(clippy::expect_used)]
    fn custom_network_injected_before_image() {
        let mut cfg = DockerEnvironmentConfig::default();
        cfg.network = DockerNetwork::Custom("my-custom-net".to_owned());
        let env = DockerEnvironment::new(&cfg, Path::new("/dummy/workdir"));
        let args = env.build_docker_run_args();

        let network_pos = args.iter().position(|a| a == "--network").expect("should have network flag");
        let image_pos = args.iter().position(|a| a == "my-image").expect("should have image");
        assert!(network_pos < image_pos);
        assert_eq!(args[network_pos + 1], "my-custom-net");
    }"""
content = content.replace(target3, replacement3)

with open("src/env/docker.rs", "w") as f:
    f.write(content)
