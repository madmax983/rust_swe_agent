with open('src/cli/mod.rs', 'r') as f:
    content = f.read()

# Revert cascade fix to match original exactly
search_casc = """    let eval_backend: crate::run::evaluate::EvaluateBackend = c.eval_backend.parse()?;
    if matches!(eval_backend, crate::run::evaluate::EvaluateBackend::None) {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "unknown --eval-backend `{}`; expected `sb-cli` or `rehearsal`", c.eval_backend
        ))));
    }"""
replace_casc = """    let eval_backend: crate::run::evaluate::EvaluateBackend = c.eval_backend.parse()?;"""
content = content.replace(search_casc, replace_casc)

with open('src/cli/mod.rs', 'w') as f:
    f.write(content)

with open('src/run/evaluate.rs', 'r') as f:
    content = f.read()

search_display = """impl std::str::FromStr for EvaluateBackend {"""
replace_display = """impl std::fmt::Display for EvaluateBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SbCli => write!(f, "sb-cli"),
            Self::None => write!(f, "none"),
            Self::Rehearsal => write!(f, "rehearsal"),
        }
    }
}

impl std::str::FromStr for EvaluateBackend {"""
content = content.replace(search_display, replace_display)

search_fromstr = """            other => Err(crate::error::Error::Config(crate::error::ConfigError::Invalid(
                format!("unknown --backend `{other}` (expected `sb-cli`, `none`, or `rehearsal`)")
            ))),"""
replace_fromstr = """            other => Err(crate::error::Error::Config(crate::error::ConfigError::Invalid(
                format!("unknown evaluator backend `{other}` (expected `sb-cli`, `none`, or `rehearsal`)")
            ))),"""
content = content.replace(search_fromstr, replace_fromstr)

with open('src/run/evaluate.rs', 'w') as f:
    f.write(content)

with open('src/run/evaluator_selftest.rs', 'r') as f:
    content = f.read()

search_selftest = """        evaluator_backend: format!("{:?}", args.backend).to_lowercase(),"""
replace_selftest = """        evaluator_backend: args.backend.to_string(),"""
content = content.replace(search_selftest, replace_selftest)

search_selftest2 = """        "unknown --backend {:?}: accepted values are `none` and `sb-cli`",
        format!("{:?}", args.backend).to_lowercase()"""
replace_selftest2 = """        "unknown --backend {}: accepted values are `none` and `sb-cli`",
        args.backend"""
content = content.replace(search_selftest2, replace_selftest2)

with open('src/run/evaluator_selftest.rs', 'w') as f:
    f.write(content)
