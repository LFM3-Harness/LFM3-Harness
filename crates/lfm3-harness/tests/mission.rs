use std::collections::BTreeMap;

use lfm3_harness::{
    mission::{load_missions, InputKind, MissionManifest},
    AcceptanceSpec, ArtifactId, CheckSpec, ExecutionStrategy, InputArtifact, PlanDraft, PlanError,
    PlannedStep, RequiredArtifact, StrategyAction,
};
use serde_json::{json, Value};

fn missions_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../missions")
}

fn desk_task() -> MissionManifest {
    load_missions(missions_dir())
        .expect("missions load")
        .remove("desk_task")
        .expect("desk_task exists")
}

fn step(index: u32, tool: &str, checks: Vec<CheckSpec>) -> PlannedStep {
    PlannedStep {
        index,
        title: format!("Step {index}"),
        criterion: format!("Step {index} finishes the requested edit visibly"),
        checks,
        tool: Some(tool.to_owned()),
    }
}

fn exists_check(path: &str) -> CheckSpec {
    CheckSpec::new("file_exists", [("path", Value::String(path.to_owned()))])
}

fn plan() -> PlanDraft {
    PlanDraft {
        steps: vec![
            step(1, "list_files", vec![exists_check("notes.md")]),
            step(2, "write_file", vec![exists_check("notes.md")]),
        ],
    }
}

fn inputs() -> BTreeMap<String, InputArtifact> {
    let mut inputs = BTreeMap::new();
    inputs.insert(
        "goal".to_owned(),
        InputArtifact {
            id: ArtifactId::new(),
            sha256: "a".repeat(64),
        },
    );
    inputs
}

fn frozen() -> lfm3_harness::FrozenAcceptance {
    desk_task()
        .compile_acceptance("Update the workspace notes", inputs(), plan())
        .expect("acceptance freezes")
}

#[test]
fn all_four_manifests_load_and_keep_their_capabilities() {
    let missions = load_missions(missions_dir()).expect("missions load");
    let names: Vec<&str> = missions.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        vec!["desk_task", "doc_extract", "screen_qa", "voice_intake"]
    );

    let desk = &missions["desk_task"];
    assert_eq!(
        desk.tools,
        vec!["list_files", "read_file", "write_file", "edit_file"]
    );
    assert!(!desk.tools.iter().any(|tool| tool == "run_command"));
    assert!(desk.command_templates.is_empty());
    assert_eq!(
        missions["voice_intake"].inputs["schema"].kind,
        InputKind::Json
    );
    assert!(missions["voice_intake"].inputs["schema"].schema.is_some());
}

#[test]
fn manifests_reject_tools_the_engine_does_not_implement() {
    for tool in ["remember", "recall", "fetch_url", "run_command", "http_get"] {
        let mut manifest = desk_task();
        manifest.tools.push(tool.to_owned());
        let raw = serde_json::to_string(&manifest).unwrap();
        let error = MissionManifest::from_json(&raw).expect_err("tool must be rejected");
        assert!(
            matches!(error, PlanError::InvalidManifest(ref message) if message.contains(tool)),
            "{tool:?} produced {error:?}"
        );
    }
}

#[test]
fn plans_cannot_use_tools_outside_the_mission() {
    let manifest = desk_task();
    let mut draft = plan();
    draft.steps[1].tool = Some("transcribe".to_owned());
    let error = manifest.validate_plan(&draft).expect_err("must reject");
    assert!(matches!(
        error,
        PlanError::UnknownTool { index: 2, ref tool } if tool == "transcribe"
    ));
}

#[test]
fn plans_need_ordered_indexes_checks_and_specific_criteria() {
    let manifest = desk_task();

    let mut no_checks = plan();
    no_checks.steps[0].checks.clear();
    assert!(matches!(
        manifest.validate_plan(&no_checks),
        Err(PlanError::NoChecks { index: 1 })
    ));

    let mut vague = plan();
    vague.steps[0].criterion = "do it".to_owned();
    assert!(matches!(
        manifest.validate_plan(&vague),
        Err(PlanError::VagueCriterion { index: 1 })
    ));

    let mut zero_index = plan();
    zero_index.steps[0].index = 0;
    assert!(matches!(
        manifest.validate_plan(&zero_index),
        Err(PlanError::InvalidStepIndex { index: 0 })
    ));

    let mut duplicated = plan();
    duplicated.steps[1].index = 1;
    assert!(matches!(
        manifest.validate_plan(&duplicated),
        Err(PlanError::InvalidStepIndex { index: 1 })
    ));

    let too_many = PlanDraft {
        steps: (1..=9)
            .map(|index| step(index, "write_file", vec![exists_check("notes.md")]))
            .collect(),
    };
    assert!(matches!(
        manifest.validate_plan(&too_many),
        Err(PlanError::TooManySteps {
            actual: 9,
            maximum: 8
        })
    ));
}

#[test]
fn checks_must_be_known_and_cannot_be_vacuous() {
    let manifest = desk_task();
    let mut unknown = plan();
    unknown.steps[0].checks = vec![CheckSpec::new(
        "d1_noul",
        [("criterion", json!("looks right"))],
    )];
    assert!(matches!(
        manifest.validate_plan(&unknown),
        Err(PlanError::UnknownCheck { index: 1, ref kind }) if kind == "d1_noul"
    ));

    for (check, expected) in [
        (CheckSpec::new("file_exists", []), "path"),
        (
            CheckSpec::new("content_matches", [("path", json!("a.md"))]),
            "contains",
        ),
        (
            CheckSpec::new(
                "content_matches",
                [("path", json!("a.md")), ("all_of", json!([]))],
            ),
            "all_of",
        ),
        (
            CheckSpec::new("json_schema", [("path", json!("a.json"))]),
            "schema",
        ),
        (
            CheckSpec::new(
                "numeric_consistency",
                [("path", json!("a.json")), ("total", json!("t"))],
            ),
            "sum",
        ),
        (
            CheckSpec::new("report_complete", [("fields", json!([]))]),
            "fields",
        ),
        (
            CheckSpec::new("file_exists", [("path", json!("  "))]),
            "path",
        ),
    ] {
        let kind = check.kind.clone();
        let mut draft = plan();
        draft.steps[0].checks = vec![check];
        let error = manifest.validate_plan(&draft).expect_err("must reject");
        assert!(
            matches!(
                error,
                PlanError::InvalidCheck { index: 1, ref message }
                    if message.contains(expected) || message.contains(&kind)
            ),
            "{kind} produced {error:?}"
        );
    }
}

#[test]
fn acceptance_hash_is_stable_for_equal_plans_and_inputs() {
    let inputs = BTreeMap::from([(
        "goal".to_owned(),
        InputArtifact {
            id: ArtifactId::new(),
            sha256: "b".repeat(64),
        },
    )]);
    let manifest = desk_task();
    let first = manifest
        .compile_acceptance("Update the workspace notes", inputs.clone(), plan())
        .unwrap();
    let second = manifest
        .compile_acceptance("Update the workspace notes", inputs, plan())
        .unwrap();
    assert_eq!(first.hash(), second.hash());
    assert_eq!(first.hash().len(), 64);
    first.verify_hash().expect("hash verifies");

    let mut reversed = plan();
    reversed.steps.reverse();
    let third = manifest
        .compile_acceptance(
            "Update the workspace notes",
            frozen().spec().inputs.clone(),
            reversed,
        )
        .unwrap();
    assert_ne!(
        first.hash(),
        third.hash(),
        "a different input artifact identity must change the contract"
    );

    let stable = manifest
        .compile_acceptance("Update the workspace notes", first.spec().inputs.clone(), {
            let mut again = plan();
            again.steps.reverse();
            again
        })
        .unwrap();
    assert_eq!(first.hash(), stable.hash(), "canonical step order wins");
}

#[test]
fn acceptance_hash_changes_when_the_tests_change() {
    let manifest = desk_task();
    let base = frozen();
    let mut altered = plan();
    altered.steps[1].checks = vec![CheckSpec::new(
        "content_matches",
        [("path", json!("notes.md")), ("contains", json!("done"))],
    )];
    let changed = manifest
        .compile_acceptance(
            "Update the workspace notes",
            base.spec().inputs.clone(),
            altered,
        )
        .unwrap();
    assert_ne!(base.hash(), changed.hash());
}

#[test]
fn replanning_cannot_swap_out_the_frozen_acceptance() {
    let acceptance = frozen();
    let hash = acceptance.hash().to_owned();

    let valid = ExecutionStrategy {
        acceptance_hash: hash.clone(),
        actions: vec![StrategyAction {
            step_index: 1,
            tool: "list_files".to_owned(),
            args: BTreeMap::from([("path".to_owned(), json!("."))]),
        }],
    };
    acceptance
        .validate_strategy(&valid)
        .expect("valid strategy");

    let mut mismatched = valid.clone();
    mismatched.acceptance_hash = "c".repeat(64);
    assert!(matches!(
        acceptance.validate_strategy(&mismatched),
        Err(PlanError::AcceptanceHashMismatch)
    ));

    let mut unknown_step = valid.clone();
    unknown_step.actions[0].step_index = 7;
    assert!(matches!(
        acceptance.validate_strategy(&unknown_step),
        Err(PlanError::UnknownStrategyStep(7))
    ));

    let mut escaped_tool = valid;
    escaped_tool.actions[0].tool = "run_command".to_owned();
    assert!(matches!(
        acceptance.validate_strategy(&escaped_tool),
        Err(PlanError::UnknownTool { index: 1, ref tool }) if tool == "run_command"
    ));
}

#[test]
fn acceptance_rejects_bad_input_hashes_and_output_paths() {
    let manifest = desk_task();
    let inputs = BTreeMap::from([(
        "goal".to_owned(),
        InputArtifact {
            id: ArtifactId::new(),
            sha256: "not-a-hash".to_owned(),
        },
    )]);
    assert!(matches!(
        manifest.compile_acceptance("Update the workspace notes", inputs, plan()),
        Err(PlanError::InvalidArtifactHash(_))
    ));

    let mut escaping = desk_task();
    escaping.output.required_artifacts = vec![RequiredArtifact {
        path: "../escape.md".to_owned(),
        non_empty: true,
    }];
    let raw = serde_json::to_string(&escaping).unwrap();
    assert!(matches!(
        MissionManifest::from_json(&raw),
        Err(PlanError::InvalidPath(_))
    ));

    let mut absolute = desk_task();
    absolute.output.required_artifacts = vec![RequiredArtifact {
        path: "C:/Windows/system32/evil.dll".to_owned(),
        non_empty: true,
    }];
    let raw = serde_json::to_string(&absolute).unwrap();
    assert!(matches!(
        MissionManifest::from_json(&raw),
        Err(PlanError::InvalidPath(_))
    ));
}

#[test]
fn acceptance_spec_rejects_empty_goals_and_step_gaps() {
    let manifest = desk_task();
    assert!(matches!(
        manifest.compile_acceptance("   ", inputs(), plan()),
        Err(PlanError::EmptyGoal)
    ));

    let mut gapped = plan();
    gapped.steps[1].index = 3;
    let spec = AcceptanceSpec {
        schema_version: 1,
        mission: "desk_task".to_owned(),
        mission_version: 1,
        goal: "Update the workspace notes".to_owned(),
        inputs: inputs(),
        steps: gapped.steps,
        output: Default::default(),
        allowed_tools: manifest.tools.iter().cloned().collect(),
        allowed_checks: manifest.checks.iter().cloned().collect(),
        max_steps: 8,
        max_attempts: 3,
    };
    assert!(matches!(
        spec.freeze(),
        Err(PlanError::InvalidStepIndex { index: 3 })
    ));
}
