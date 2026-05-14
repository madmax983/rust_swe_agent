1. **Analyze comment:** The user suggests replacing the `apply_config_overrides!` macro with a trait-based approach. We should define a `ConfigOverrides` trait that both `args::MiniCmd` and `args::SwebenchCmd` implement, and then have a function apply the overrides.
2. **Review codebase:** Let's look at `src/cli/args.rs` and `src/cli/mod.rs` to see how to implement this trait.
