#[test]
fn test_cli_parsing_policy_impact() {
    use maxwells_daemon::cli::{Cli, Command};
    use maxwells_daemon::cli::args::BenchCmd;
    use clap::Parser;

    let args = Cli::try_parse_from(["max", "bench", "policy-impact", "--sweep", "some_sweep_dir"]);
    assert!(args.is_ok(), "Failed to parse args: {:?}", args.err());
    let cli = args.unwrap();
    match cli.command {
        Command::Bench { cmd } => match cmd {
            BenchCmd::PolicyImpact(cmd_args) => {
                assert_eq!(cmd_args.sweep, std::path::PathBuf::from("some_sweep_dir"));
                assert_eq!(cmd_args.format, "text");
            }
            _ => panic!("Expected BenchCmd::PolicyImpact"),
        },
        _ => panic!("Expected Command::Bench"),
    }
}
