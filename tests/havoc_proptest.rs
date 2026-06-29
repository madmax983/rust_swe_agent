use proptest::prelude::*;

proptest! {
    #[test]
    fn test_cap_canonical(s in ".*", cap in 0usize..1000) {
        let (res, truncated) = maxwells_daemon::fingerprint::cap_canonical(&s, cap);
        assert!(res.len() <= cap + "[truncated]".len());
        if truncated {
            assert!(res.ends_with("[truncated]"));
        }
    }

    #[test]
    fn test_truncate_chars(s in ".*", max_chars in 0usize..1000) {
        let res = maxwells_daemon::run::triage::truncate_chars(&s, max_chars);
        assert!(res.chars().count() <= max_chars + 3);
    }

    #[test]
    fn test_cap_rationale(s in ".*") {
        let _ = maxwells_daemon::agent::confirm::cap_rationale(&s);
    }

    #[test]
    fn test_normalize_signature_text(s in ".*") {
         let _ = maxwells_daemon::run::triage::normalize_signature_text(&s);
    }

    #[test]
    fn test_action_hash(s in ".*") {
        let _ = maxwells_daemon::stagnation::action_hash(&s);
    }

    #[test]
    fn test_canonicalize_action(s in ".*") {
        let _ = maxwells_daemon::stagnation::canonicalize_action(&s);
    }

    #[test]
    fn test_extract_action(s in ".*") {
        let _ = maxwells_daemon::agent::parse::extract_action(&s);
    }

    #[test]
    fn test_extract_action_for_tools(s in ".*") {
        let tools = vec!["bash".to_string(), "read_file".to_string()];
        let _ = maxwells_daemon::agent::parse::extract_action_for_tools(&s, &tools);
    }

    #[test]
    fn test_resolve_trajectory_path(s in ".*", id in ".*") {
        let _ = maxwells_daemon::run::triage::resolve_trajectory_path(std::path::Path::new(&s), &id);
    }

    #[test]
    fn test_wrap_untrusted(s in ".*") {
        let _ = maxwells_daemon::prompt_guard::PromptGuard::wrap(maxwells_daemon::prompt_guard::UntrustedKind::ToolOutput, &s);
    }

    #[test]
    fn test_normalize_redaction_markers(s in "\\PC*") {
        let _ = maxwells_daemon::fingerprint::normalize_redaction_markers(&s);
    }

    #[test]
    fn test_f64_math(v in proptest::num::f64::ANY) {
        let _ = maxwells_daemon::run::evaluate::round_dp(v, 2);
    }

    #[test]
    fn test_day_bucket(s in proptest::option::of(".*")) {
        let _ = maxwells_daemon::run::ledger::day_bucket(s.as_deref());
    }

    #[test]
    fn test_compute_cache_hit_rate(a in proptest::num::u64::ANY, b in proptest::num::u64::ANY, c in proptest::num::u64::ANY) {
        let _ = maxwells_daemon::run::cache_stats::compute_cache_hit_rate(a, b, c);
    }
}
