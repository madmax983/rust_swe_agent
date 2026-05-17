#[test]
fn step_idx_next_saturates_on_max() {
    let mut step = maxwells_daemon::ids::StepIdx(u32::MAX);
    step = step.next();
    assert_eq!(step.get(), u32::MAX);
}
