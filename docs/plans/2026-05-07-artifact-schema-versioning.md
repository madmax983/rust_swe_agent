# Artifact Schema Versioning Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add explicit, typed schema metadata to persisted run artifacts and make public reader commands reject unsupported future major versions before computing metrics.

**Architecture:** Introduce a small `artifact` module with `ArtifactKind`, `ArtifactSchemaVersion`, and compatibility classification. Producers write top-level `artifact_kind` and `schema_version` fields through versioned JSON wrappers, while readers validate raw JSON before deserializing payloads so legacy artifacts still load with warnings and future major versions fail fast.

**Tech Stack:** Rust 2024, `serde`, `serde_json`, existing CLI integration tests.

---

### Task 1: Typed Schema Core

**Files:**
- Create: `src/artifact.rs`
- Modify: `src/lib.rs`
- Test: `tests/artifact_schema.rs`

**Steps:**
1. Write failing tests for current, legacy, future-major, and kind-mismatch classification.
2. Run `cargo test --test artifact_schema artifact_classifier -- --nocapture` and verify it fails because `artifact` does not exist.
3. Implement `ArtifactKind`, `ArtifactSchemaVersion`, `ArtifactHeader`, `ArtifactCompatibility`, and classifier helpers.
4. Re-run the focused test and verify green.

### Task 2: Producer Metadata

**Files:**
- Modify: `src/trajectory/mod.rs`
- Modify: `src/run/swebench.rs`
- Modify: `src/run/evaluate.rs`
- Modify: `src/run/forecast.rs`
- Test: `tests/artifact_schema.rs`, existing producer tests

**Steps:**
1. Add failing tests that serialize trajectory, `results.json`, `evaluation.json`, forecast JSON, preflight JSON, and prediction metadata.
2. Implement versioned serialization wrappers and prediction metadata files without modifying SWE-bench JSONL prediction rows.
3. Verify focused tests pass.

### Task 3: Reader Gates

**Files:**
- Modify: `src/run/compare.rs`
- Modify: `src/run/inspect.rs`
- Modify: `src/run/tail.rs`
- Modify: `src/run/trajectory_diff.rs`
- Test: `tests/artifact_schema.rs`, `tests/bench_compare.rs`, `tests/bench_inspect.rs`, `tests/bench_tail.rs`

**Steps:**
1. Add failing tests that public read commands warn for legacy artifacts and fail before metrics for unsupported future major versions.
2. Gate `load_sweep`, evaluation loading, trajectory scans, inspect reports, tail snapshots, and trajectory diff loading through the classifier.
3. Add compare artifact mismatch fields and render them before resolved-rate and regression numbers.
4. Verify focused read-command tests pass.

### Task 4: Contract Docs And Fixtures

**Files:**
- Create: `docs/artifact-contract.md`
- Create: `tests/fixtures/artifact_schema/...`
- Test: `tests/artifact_schema.rs`

**Steps:**
1. Add checked-in legacy, current, and unsupported-future fixtures.
2. Document artifact kinds, current version, required fields, optional fields, and compatibility policy.
3. Add fixture tests for supported load and unsupported-future failure.

### Task 5: Verification

**Commands:**
- `cargo fmt`
- `cargo test --test artifact_schema`
- `cargo test --test bench_compare --test bench_inspect --test bench_tail --test bench_forecast`
- `cargo test`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `rg -n "TODO|FIXME|Stub:" src tests docs`

**Expected:** All tests and clippy pass; any existing TODOs are reviewed and unrelated ones are not treated as new work.
