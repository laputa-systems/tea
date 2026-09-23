//! Fail-closed command surface for Tea's optional free-Zen live verification lane.
//!
//! This is intentionally an example target rather than a normal `tea` command. It never runs a
//! provider by default and never exposes a generic provider/model option. Once the durable
//! six-case driver is available, it must receive `RestrictedZenConsumer` handles from the single
//! factory constructed below; it must not construct an adapter itself.

use std::env;
use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tea_agent::verification::{
    LiveChildScenario, LiveCompactionScenario, LiveEvolutionScenario,
    run_controlled_recovery_live_case, run_headless_live_case, run_live_child_scenario,
    run_live_compaction_scenario, run_live_evolution_scenario, ControlledRecoveryLiveCase,
    FreeZenCatalogEvidence, HeadlessLiveCase, RestrictedZenFactory, VerificationConsumer,
    ZEN_FREE_MODEL_ID, ZEN_PROVIDER_ID, ZEN_RESPONSES_ENDPOINT,
};
use tea_protocol::JsonValue;

const REPORT_SCHEMA: &str = "tea-live-verification-report/v1";

struct Arguments {
    catalog_evidence: PathBuf,
    ledger: PathBuf,
    out: PathBuf,
    live: bool,
    synthetic_or_public_fixtures: bool,
    run_counterparts: bool,
    tea_home: Option<PathBuf>,
    workspace: Option<PathBuf>,
}

struct OneShotChildArguments {
    catalog_evidence: PathBuf,
    ledger: PathBuf,
    tea_home: PathBuf,
    workspace: PathBuf,
}

#[derive(Clone, Copy)]
struct VerificationCase {
    consumer: VerificationConsumer,
    id: &'static str,
    counterpart: &'static [&'static str],
}

struct CounterpartOutcome {
    status: &'static str,
    detail: Option<String>,
}

struct LiveOutcome {
    status: &'static str,
    detail: Option<String>,
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(message) => {
            eprintln!("live verification BLOCKED: {message}");
            std::process::exit(2);
        }
    }
}

fn run() -> Result<i32, String> {
    if env::args_os().skip(1).any(|argument| argument == "--one-shot-child") {
        return run_one_shot_child();
    }
    let args = parse_arguments()?;
    let evidence = FreeZenCatalogEvidence::read(&args.catalog_evidence).map_err(|error| error.to_string())?;
    let cases = verification_cases();
    let counterpart_outcomes = if args.run_counterparts {
        run_counterparts(&cases)
    } else {
        cases
            .iter()
            .map(|_| CounterpartOutcome {
                status: "NOT_RUN",
                detail: Some("pass --run-counterparts to execute this offline oracle".into()),
            })
            .collect()
    };
    let mut live_outcomes = cases
        .iter()
        .map(|_| LiveOutcome {
            status: "BLOCKED",
            detail: Some("pass --live after confirming catalog evidence and data-use terms".into()),
        })
        .collect::<Vec<_>>();
    let blocker;
    let mut budget = JsonValue::Null;
    if !args.live {
        blocker = Some("pass --live only after reviewing the current official Zen catalog evidence".to_owned());
    } else if !args.synthetic_or_public_fixtures {
        blocker = Some("pass --synthetic-or-public-fixtures to acknowledge the route's data-use terms".to_owned());
        live_outcomes.iter_mut().for_each(|outcome| {
            outcome.detail = Some(
                "live execution requires --synthetic-or-public-fixtures acknowledgement".into(),
            )
        });
    } else {
        match take_opencode_api_key() {
            Err(_) => {
                blocker = Some(
                    "OPENCODE_API_KEY is unavailable at the explicit host secret boundary; no inference was sent"
                        .to_owned(),
                );
                live_outcomes.iter_mut().for_each(|outcome| {
                    outcome.detail = Some(
                        "OPENCODE_API_KEY is unavailable at the explicit host secret boundary".into(),
                    )
                });
            }
            Ok(api_key) => match RestrictedZenFactory::new(
                api_key.clone(),
                evidence.clone(),
                args.ledger.clone(),
                NonZeroU64::new(2_048).expect("fixed output allowance is nonzero"),
            ) {
                Err(error) => {
                    let detail = format!("restricted factory rejected live setup: {error}");
                    blocker = Some(detail.clone());
                    live_outcomes.iter_mut().for_each(|outcome| {
                        outcome.detail = Some(detail.clone());
                    });
                }
                Ok(factory) => {
                    live_outcomes = run_live_cases(&args, &factory, &cases, &api_key);
                    budget = budget_json(
                        &factory
                            .reload_budget_snapshot()
                            .map_err(|error| error.to_string())?,
                    );
                    blocker = Some(
                        "headless transport cases executed only with explicit synthetic fixtures; inspect each semantic status before claiming a live pass".to_owned(),
                    );
                }
            },
        }
    }
    let report = JsonValue::object([
        ("schema_version", JsonValue::from(REPORT_SCHEMA)),
        ("status", JsonValue::from("BLOCKED")),
        ("blocker", JsonValue::from(blocker.expect("all paths set a blocker"))),
        ("catalog_source", JsonValue::from("https://opencode.ai/docs/zen")),
        ("checked_on", JsonValue::from(evidence.checked_on())),
        ("provider", JsonValue::from(ZEN_PROVIDER_ID)),
        ("model", JsonValue::from(ZEN_FREE_MODEL_ID)),
        ("endpoint", JsonValue::from(ZEN_RESPONSES_ENDPOINT)),
        ("data_use_source", JsonValue::from(evidence.data_use_source())),
        ("data_use_summary", JsonValue::from(evidence.data_use_summary())),
        ("budget", budget),
        (
            "cases",
            JsonValue::Array(
                cases
                    .iter()
                    .zip(&counterpart_outcomes)
                    .map(|(case, outcome)| {
                        JsonValue::object([
                            ("id", JsonValue::from(case.id)),
                            ("consumer", JsonValue::from(consumer_name(case.consumer))),
                            ("status", JsonValue::from("BLOCKED")),
                            (
                                "deterministic_counterpart",
                                JsonValue::from(case.counterpart.join(" ")),
                            ),
                            ("offline_oracle_status", JsonValue::from(outcome.status)),
                            (
                                "offline_oracle_detail",
                                outcome
                                    .detail
                                    .as_ref()
                                    .map_or(JsonValue::Null, |detail| JsonValue::from(detail.clone())),
                            ),
                            ("live_transport_status", JsonValue::from(live_outcomes
                                .get(case_index(&cases, case))
                                .expect("case and live outcome length match")
                                .status)),
                            (
                                "live_transport_detail",
                                live_outcomes
                                    .get(case_index(&cases, case))
                                    .expect("case and live outcome length match")
                                    .detail
                                    .as_ref()
                                    .map_or(JsonValue::Null, |detail| JsonValue::from(detail.clone())),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
    ]);
    write_report(&args.out, &report)?;
    println!("live verification BLOCKED; sanitized report: {}", args.out.display());
    Ok(if counterpart_outcomes.iter().any(|outcome| outcome.status == "FAILED") {
        1
    } else {
        2
    })
}

fn take_opencode_api_key() -> Result<String, String> {
    let api_key = env::var("OPENCODE_API_KEY").map_err(|_| {
        "OPENCODE_API_KEY is unavailable at the explicit host secret boundary; no inference was sent"
            .to_owned()
    })?;
    env::remove_var("OPENCODE_API_KEY");
    Ok(api_key)
}

fn run_one_shot_child() -> Result<i32, String> {
    let arguments = parse_one_shot_child_arguments()?;
    let api_key = take_opencode_api_key()?;
    let evidence = FreeZenCatalogEvidence::read(&arguments.catalog_evidence)
        .map_err(|error| error.to_string())?;
    let factory = RestrictedZenFactory::new(
        api_key,
        evidence,
        arguments.ledger,
        NonZeroU64::new(2_048).expect("fixed output allowance is nonzero"),
    )
    .map_err(|error| error.to_string())?;
    let consumer = factory.consumer(VerificationConsumer::Root);
    let outcome = run_headless_live_case(HeadlessLiveCase {
        tea_home: &arguments.tea_home,
        workspace: &arguments.workspace,
        role: VerificationConsumer::Root,
        consumer: &consumer,
        compactor: None,
        one_shot: true,
        passive_reopen: false,
        expected_response: Some("READY"),
        prompt: live_prompt("headless-and-one-shot"),
    })
    .map_err(|error| error.to_string())?;
    if outcome.operation_completed
        && outcome.durable_state_verified
        && outcome.response_oracle_verified
    {
        Ok(0)
    } else {
        Err("one-shot child did not satisfy its durable synthetic oracle".into())
    }
}

fn verification_cases() -> [VerificationCase; 6] {
    [
        VerificationCase {
            consumer: VerificationConsumer::Root,
            id: "synthetic-coding",
            counterpart: &["cargo", "test", "-p", "tea-core", "--test", "coding_capabilities", "--locked"],
        },
        VerificationCase {
            consumer: VerificationConsumer::Root,
            id: "headless-and-one-shot",
            counterpart: &[
                "cargo",
                "run",
                "--quiet",
                "-p",
                "tea-core",
                "--features",
                "fixture-runner",
                "--bin",
                "tea-fixtures",
                "--locked",
                "--",
                "crates/tea-core/fixtures/declarative/single-turn-text.json",
            ],
        },
        VerificationCase {
            consumer: VerificationConsumer::Root,
            id: "interrupted-reopen",
            counterpart: &[
                "cargo",
                "run",
                "--quiet",
                "-p",
                "tea-core",
                "--features",
                "fixture-runner",
                "--bin",
                "tea-fixtures",
                "--locked",
                "--",
                "crates/tea-core/fixtures/declarative/model-stream-cancellation-reuse.json",
            ],
        },
        VerificationCase {
            consumer: VerificationConsumer::Compaction,
            id: "forced-compaction",
            counterpart: &["cargo", "test", "-p", "tea-core", "--lib", "--locked", "compaction"],
        },
        VerificationCase {
            consumer: VerificationConsumer::CandidateEvaluation,
            id: "luau-activation-rollback",
            counterpart: &["cargo", "test", "-p", "tea-luau", "--locked"],
        },
        VerificationCase {
            consumer: VerificationConsumer::Child,
            id: "isolated-children",
            counterpart: &["cargo", "test", "-p", "tea-core", "--lib", "--locked", "subagent"],
        },
    ]
}

fn run_counterparts(cases: &[VerificationCase]) -> Vec<CounterpartOutcome> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("tea-agent source has a workspace root");
    cases
        .iter()
        .map(|case| {
            if let Some(expected) = fixture_expected(case.id) {
                return run_fixture_oracle(counterpart_command(case, root), &root.join(expected));
            }
            let result = counterpart_command(case, root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match result {
                Ok(status) if status.success() => CounterpartOutcome {
                    status: "PASSED",
                    detail: None,
                },
                Ok(status) => CounterpartOutcome {
                    status: "FAILED",
                    detail: Some(format!("offline oracle exited with {}", status.code().unwrap_or(-1))),
                },
                Err(error) => CounterpartOutcome {
                    status: "FAILED",
                    detail: Some(format!("offline oracle could not start: {error}")),
                },
            }
        })
        .collect()
}

fn counterpart_command(case: &VerificationCase, root: &Path) -> Command {
    let (program, arguments) = case
        .counterpart
        .split_first()
        .expect("every verification counterpart has a program");
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(root)
        .env_remove("OPENCODE_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("CODEX_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .stdin(Stdio::null());
    command
}

fn fixture_expected(case_id: &str) -> Option<&'static str> {
    match case_id {
        "headless-and-one-shot" => {
            Some("crates/tea-core/fixtures/expected/single-turn-text.json")
        }
        "interrupted-reopen" => {
            Some("crates/tea-core/fixtures/expected/model-stream-cancellation-reuse.json")
        }
        _ => None,
    }
}

fn run_fixture_oracle(mut command: Command, expected_path: &Path) -> CounterpartOutcome {
    let output = match command.output() {
        Ok(output) if output.status.success() => output,
        Ok(status) => {
            return CounterpartOutcome {
                status: "FAILED",
                detail: Some(format!(
                    "synthetic fixture exited with {}",
                    status.status.code().unwrap_or(-1)
                )),
            };
        }
        Err(_) => {
            return CounterpartOutcome {
                status: "FAILED",
                detail: Some("synthetic fixture could not start".into()),
            };
        }
    };
    let actual = std::str::from_utf8(&output.stdout)
        .ok()
        .and_then(|source| JsonValue::parse(source).ok());
    let expected = fs::read_to_string(expected_path)
        .ok()
        .and_then(|source| JsonValue::parse(&source).ok());
    match (actual, expected) {
        (Some(actual), Some(expected)) if actual == expected => CounterpartOutcome {
            status: "PASSED",
            detail: None,
        },
        _ => CounterpartOutcome {
            status: "FAILED",
            detail: Some("synthetic fixture output differs from its checked-in oracle".into()),
        },
    }
}

fn run_live_cases(
    arguments: &Arguments,
    factory: &RestrictedZenFactory,
    cases: &[VerificationCase],
    api_key: &str,
) -> Vec<LiveOutcome> {
    let (Some(tea_home_root), Some(workspace_root)) =
        (arguments.tea_home.as_deref(), arguments.workspace.as_deref())
    else {
        return blocked_live_cases(
            cases,
            "live execution requires explicit existing --tea-home and --workspace directories",
        );
    };
    if !tea_home_root.is_dir() || !workspace_root.is_dir() {
        return blocked_live_cases(
            cases,
            "live execution requires --tea-home and --workspace to name existing directories",
        );
    }
    cases
        .iter()
        .map(|case| {
            let tea_home = tea_home_root.join(case.id);
            let workspace = workspace_root.join(case.id);
            if let Err(error) = fs::create_dir(&tea_home).and_then(|()| fs::create_dir(&workspace)) {
                return LiveOutcome {
                    status: "FAILED",
                    detail: Some(format!("could not prepare explicit case directories: {error}")),
                };
            }
            if let Err(error) = prepare_public_fixture(&workspace) {
                return LiveOutcome {
                    status: "FAILED",
                    detail: Some(format!("could not prepare public synthetic fixture: {error}")),
                };
            }
            if case.id == "headless-and-one-shot" {
                return match run_one_shot_child_process(
                    arguments,
                    &tea_home,
                    &workspace,
                    api_key,
                ) {
                    Ok(()) => LiveOutcome {
                        status: "SEMANTIC_PASSED",
                        detail: Some(
                            "a separately invoked guarded one-shot executable completed its disposable durable synthetic oracle"
                                .into(),
                        ),
                    },
                    Err(error) => LiveOutcome {
                        status: "FAILED",
                        detail: Some(format!("guarded one-shot child failed: {error}")),
                    },
                };
            }
            if case.id == "forced-compaction" {
                let root = factory.consumer(VerificationConsumer::Root);
                let compactor = factory.consumer(VerificationConsumer::Compaction);
                return match run_live_compaction_scenario(LiveCompactionScenario {
                    tea_home: &tea_home,
                    workspace: &workspace,
                    root: &root,
                    compactor: &compactor,
                    critical_fact: "public-compaction-retention-marker",
                }) {
                    Ok(outcome)
                        if outcome.compaction_request_observed
                            && outcome.checkpoint_committed
                            && outcome.critical_fact_retained
                            && outcome.durable_state_verified => LiveOutcome {
                                status: "SEMANTIC_PASSED",
                                detail: Some(
                                    "forced automatic compaction committed linked request material and retained its synthetic marker across passive reopen"
                                        .into(),
                                ),
                            },
                    Ok(_) => LiveOutcome {
                        status: "FAILED",
                        detail: Some(
                            "forced compaction did not satisfy its linked checkpoint and retention oracle"
                                .into(),
                        ),
                    },
                    Err(error) => LiveOutcome {
                        status: "FAILED",
                        detail: Some(format!("guarded compaction transport failed: {error}")),
                    },
                };
            }
            if case.id == "isolated-children" {
                if prepare_clean_child_fixture(&workspace).is_err() {
                    return LiveOutcome {
                        status: "FAILED",
                        detail: Some(
                            "could not prepare the clean public Git fixture required for child isolation"
                                .into(),
                        ),
                    };
                }
                let root = factory.consumer(VerificationConsumer::Root);
                let child = factory.consumer(VerificationConsumer::Child);
                return match run_live_child_scenario(LiveChildScenario {
                    tea_home: &tea_home,
                    workspace: &workspace,
                    root: &root,
                    child: &child,
                    prompt: live_prompt(case.id),
                }) {
                    Ok(outcome)
                        if outcome.child_request_observed
                            && outcome.isolated_workspace_verified
                            && outcome.report_verified
                            && outcome.parent_apply_verified
                            && outcome.durable_state_verified => LiveOutcome {
                                status: "SEMANTIC_PASSED",
                                detail: Some(
                                    "injected root and child consumers produced a durable isolated delta, terminal report, and parent apply"
                                        .into(),
                                ),
                            },
                    Ok(_) => LiveOutcome {
                        status: "FAILED",
                        detail: Some(
                            "isolated child orchestration did not satisfy its durable delta, report, and apply oracle"
                                .into(),
                        ),
                    },
                    Err(error) => LiveOutcome {
                        status: "FAILED",
                        detail: Some(format!("guarded child transport failed: {error}")),
                    },
                };
            }
            if case.id == "luau-activation-rollback" {
                let candidate_evaluator = factory.consumer(VerificationConsumer::CandidateEvaluation);
                return match run_live_evolution_scenario(LiveEvolutionScenario {
                    tea_home: &tea_home,
                    workspace: &workspace,
                    candidate_evaluator: &candidate_evaluator,
                    activation_prompt: evolution_activation_prompt(),
                    use_prompt: evolution_use_prompt(),
                    rollback_prompt: evolution_rollback_prompt(),
                }) {
                    Ok(outcome)
                        if outcome.candidate_activated
                            && outcome.revised_source_used
                            && outcome.state_retained_across_rollback
                            && outcome.rollback_activated
                            && outcome.durable_state_verified => LiveOutcome {
                                status: "SEMANTIC_PASSED",
                                detail: Some(
                                    "author-mode candidate activation, revised stateful-tool use, immutable rollback, and passive reopen all satisfied their durable oracle"
                                        .into(),
                                ),
                            },
                    Ok(_) => LiveOutcome {
                        status: "FAILED",
                        detail: Some(
                            "Luau evolution did not satisfy its immutable source, state, rollback, and reopen oracle"
                                .into(),
                        ),
                    },
                    Err(error) => LiveOutcome {
                        status: "FAILED",
                        detail: Some(format!("guarded evolution transport failed: {error}")),
                    },
                };
            }
            let consumer = factory.consumer(case.consumer);
            if case.id == "interrupted-reopen" {
                return match run_controlled_recovery_live_case(ControlledRecoveryLiveCase {
                    tea_home: &tea_home,
                    workspace: &workspace,
                    consumer: &consumer,
                    interrupted_prompt: live_prompt(case.id),
                    continuation_prompt: "This is a disposable public recovery continuation. Do not use tools. Reply exactly RECOVERED.",
                    expected_continuation_response: "RECOVERED",
                }) {
                    Ok(outcome)
                        if outcome.provider_request_observed
                            && outcome.interruption_settled
                            && outcome.passive_reopen_verified
                            && outcome.continuation_completed
                            && outcome.continuation_response_verified => LiveOutcome {
                                status: "SEMANTIC_PASSED",
                                detail: Some(
                                    "controlled cancellation followed durable provider admission; passive reopen verified before a fresh, exact synthetic continuation"
                                        .into(),
                                ),
                            },
                    Ok(_) => LiveOutcome {
                        status: "FAILED",
                        detail: Some(
                            "controlled recovery did not satisfy its committed-state continuation oracle"
                                .into(),
                        ),
                    },
                    Err(error) => LiveOutcome {
                        status: "FAILED",
                        detail: Some(format!("guarded controlled recovery failed: {error}")),
                    },
                };
            }
            let outcome = run_headless_live_case(HeadlessLiveCase {
                tea_home: &tea_home,
                workspace: &workspace,
                role: case.consumer,
                consumer: &consumer,
                compactor: None,
                one_shot: false,
                passive_reopen: false,
                expected_response: None,
                prompt: live_prompt(case.id),
            });
            match outcome {
                Ok(outcome)
                    if outcome.operation_completed
                        && outcome.durable_state_verified
                        && outcome.response_oracle_verified => {
                    if case.id == "synthetic-coding"
                        && !verify_coding_fixture(&workspace)
                    {
                        LiveOutcome {
                            status: "FAILED",
                            detail: Some(
                                "synthetic coding transport settled but did not satisfy the public read/edit/test oracle"
                                    .into(),
                            ),
                        }
                    } else {
                        LiveOutcome {
                            status: "SEMANTIC_PASSED",
                            detail: Some(
                                "guarded provider completed the disposable headless operation and its output-free synthetic oracle"
                                    .into(),
                            ),
                        }
                    }
                }
                Ok(_) => LiveOutcome {
                    status: "FAILED",
                    detail: Some("headless operation did not settle and verify durably".into()),
                },
                Err(error) => LiveOutcome {
                    status: "FAILED",
                    detail: Some(format!("guarded headless transport failed: {error}")),
                },
            }
        })
        .collect()
}

fn run_one_shot_child_process(
    arguments: &Arguments,
    tea_home: &Path,
    workspace: &Path,
    api_key: &str,
) -> Result<(), String> {
    let executable = env::current_exe()
        .map_err(|error| format!("cannot locate the feature-only one-shot executable: {error}"))?;
    let status = Command::new(executable)
        .args([
            "--one-shot-child",
            "--live",
            "--synthetic-or-public-fixtures",
            "--catalog-evidence",
        ])
        .arg(&arguments.catalog_evidence)
        .arg("--ledger")
        .arg(&arguments.ledger)
        .arg("--tea-home")
        .arg(tea_home)
        .arg("--workspace")
        .arg(workspace)
        .env("OPENCODE_API_KEY", api_key)
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("CODEX_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("cannot start separate one-shot executable: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("separate one-shot executable exited with {}", status.code().unwrap_or(-1)))
    }
}

fn blocked_live_cases(cases: &[VerificationCase], detail: &str) -> Vec<LiveOutcome> {
    cases
        .iter()
        .map(|_| LiveOutcome {
            status: "BLOCKED",
            detail: Some(detail.into()),
        })
        .collect()
}

fn prepare_public_fixture(workspace: &Path) -> Result<(), std::io::Error> {
    fs::write(
        workspace.join("README.md"),
        "Disposable public Tea verification fixture. No private data is present.\n",
    )?;
    fs::write(workspace.join("fixture.txt"), "before\n")?;
    fs::write(
        workspace.join("verify-fixture.sh"),
        "#!/bin/sh\nset -eu\ntest \"$(cat fixture.txt)\" = after\n",
    )
}

fn prepare_clean_child_fixture(workspace: &Path) -> Result<(), ()> {
    for arguments in [
        ["init"].as_slice(),
        ["config", "user.name", "Tea Verification"].as_slice(),
        ["config", "user.email", "verification@example.invalid"].as_slice(),
        ["add", "README.md", "fixture.txt", "verify-fixture.sh"].as_slice(),
        ["commit", "-m", "public verification fixture"].as_slice(),
    ] {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(workspace)
            .env_remove("OPENCODE_API_KEY")
            .env_remove("OPENROUTER_API_KEY")
            .env_remove("CODEX_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|_| ())?;
        if !status.success() {
            return Err(());
        }
    }
    Ok(())
}

fn verify_coding_fixture(workspace: &Path) -> bool {
    Command::new("sh")
        .arg("verify-fixture.sh")
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn live_prompt(case_id: &str) -> &'static str {
    match case_id {
        "synthetic-coding" => {
            "This is a disposable public verification fixture. Read fixture.txt, replace its only line before with after, then run `sh verify-fixture.sh`, which exits successfully only when the file contains exactly after. Do not access paths outside this workspace. Reply with a brief confirmation."
        }
        "headless-and-one-shot" => {
            "This is a disposable public headless one-shot verification. Do not use tools. Reply exactly READY."
        }
        "interrupted-reopen" => {
            "This is a disposable public interruption/reopen verification. Do not use tools. Begin a short response, knowing the host will deliberately interrupt this request after durable provider admission."
        }
        "forced-compaction" => {
            "This is a disposable public compaction transport verification. Do not use tools. Reply exactly READY."
        }
        "luau-activation-rollback" => {
            evolution_activation_prompt()
        }
        "isolated-children" => {
            "This is a disposable public child-lane verification. Delegate the one isolated task of replacing the only line in fixture.txt from before to after. Wait for its report, then explicitly apply its reported delta. Do not access paths outside this workspace."
        }
        _ => "This is a disposable public Tea verification fixture. Do not use tools. Reply exactly READY.",
    }
}

fn evolution_activation_prompt() -> &'static str {
    concat!(
        "This is a disposable public immutable-harness verification. Use tea_harness status to find the active revision. ",
        "Then apply one candidate on that base revision that adds a new capability-free session plugin with exactly two upserted files and a registry_operations entry {\"operation\":\"add\",\"plugin_id\":\"evolution_marker\"}. ",
        "File plugins/evolution_marker/manifest.json must contain exactly {\"schema_version\":1,\"abi_version\":3,\"id\":\"evolution_marker\",\"entrypoint\":\"init.luau\",\"modules\":[\"init.luau\"],\"requested_capabilities\":[]}. ",
        "File plugins/evolution_marker/init.luau must contain exactly return { prompt_sections = {{ id = \"evolution_marker\", content = \"live verification evolution marker\" }} }. ",
        "Supply the required bounded hypothesis. The apply call must be the only tool call in its assistant batch. Do not access paths outside the immutable harness source."
    )
}

fn evolution_use_prompt() -> &'static str {
    "This is a disposable public post-activation verification. Use the todo tool exactly once to create a one-item todo plan whose text is `public evolution state marker`. Do not invoke tea_harness or access paths outside this workspace."
}

fn evolution_rollback_prompt() -> &'static str {
    "This is a disposable public immutable-harness rollback verification. Use tea_harness status and list to identify the current activated revision and the original initial revision. Stage a rollback from the current revision to that original revision with the required bounded hypothesis. The rollback call must be the only tool call in its assistant batch. Do not access paths outside the immutable harness source."
}

fn case_index(cases: &[VerificationCase], case: &VerificationCase) -> usize {
    cases
        .iter()
        .position(|candidate| candidate.id == case.id)
        .expect("verification case ids are unique")
}

fn consumer_name(consumer: VerificationConsumer) -> &'static str {
    match consumer {
        VerificationConsumer::Root => "root",
        VerificationConsumer::Child => "child",
        VerificationConsumer::Compaction => "compaction",
        VerificationConsumer::CandidateEvaluation => "candidate_evaluation",
        VerificationConsumer::Comparison => "comparison",
    }
}

fn budget_json(snapshot: &tea_agent::verification::LiveVerificationBudgetSnapshot) -> JsonValue {
    JsonValue::object([
        ("attempted_requests", JsonValue::from(snapshot.attempted_requests)),
        (
            "requested_output_tokens",
            JsonValue::from(snapshot.requested_output_tokens),
        ),
        ("active_requests", JsonValue::from(snapshot.active_requests)),
    ])
}

fn parse_one_shot_child_arguments() -> Result<OneShotChildArguments, String> {
    let mut values = env::args_os().skip(1);
    let mut catalog_evidence = None;
    let mut ledger = None;
    let mut tea_home = None;
    let mut workspace = None;
    let mut one_shot_child = false;
    let mut live = false;
    let mut synthetic_or_public_fixtures = false;
    while let Some(argument) = values.next() {
        match argument.to_str() {
            Some("--one-shot-child") => one_shot_child = true,
            Some("--live") => live = true,
            Some("--synthetic-or-public-fixtures") => synthetic_or_public_fixtures = true,
            Some("--catalog-evidence") => {
                catalog_evidence = Some(PathBuf::from(
                    values.next().ok_or("--catalog-evidence requires a path")?,
                ));
            }
            Some("--ledger") => {
                ledger = Some(PathBuf::from(values.next().ok_or("--ledger requires a path")?));
            }
            Some("--tea-home") => {
                tea_home = Some(PathBuf::from(values.next().ok_or("--tea-home requires a path")?));
            }
            Some("--workspace") => {
                workspace = Some(PathBuf::from(values.next().ok_or("--workspace requires a path")?));
            }
            _ => {
                return Err(format!(
                    "unknown one-shot child argument: {}",
                    argument.to_string_lossy()
                ));
            }
        }
    }
    if !one_shot_child || !live || !synthetic_or_public_fixtures {
        return Err(
            "one-shot child requires --one-shot-child --live --synthetic-or-public-fixtures"
                .into(),
        );
    }
    Ok(OneShotChildArguments {
        catalog_evidence: catalog_evidence.ok_or("--catalog-evidence is required")?,
        ledger: ledger.ok_or("--ledger is required")?,
        tea_home: tea_home.ok_or("--tea-home is required")?,
        workspace: workspace.ok_or("--workspace is required")?,
    })
}

fn parse_arguments() -> Result<Arguments, String> {
    let mut values = env::args_os().skip(1);
    let mut catalog_evidence = None;
    let mut ledger = None;
    let mut out = None;
    let mut live = false;
    let mut synthetic_or_public_fixtures = false;
    let mut run_counterparts = false;
    let mut tea_home = None;
    let mut workspace = None;
    while let Some(argument) = values.next() {
        match argument.to_str() {
            Some("--catalog-evidence") => {
                catalog_evidence = Some(PathBuf::from(
                    values.next().ok_or("--catalog-evidence requires a path")?,
                ));
            }
            Some("--ledger") => {
                ledger = Some(PathBuf::from(values.next().ok_or("--ledger requires a path")?));
            }
            Some("--out") => {
                out = Some(PathBuf::from(values.next().ok_or("--out requires a path")?));
            }
            Some("--live") => live = true,
            Some("--synthetic-or-public-fixtures") => synthetic_or_public_fixtures = true,
            Some("--run-counterparts") => run_counterparts = true,
            Some("--tea-home") => {
                tea_home = Some(PathBuf::from(values.next().ok_or("--tea-home requires a path")?));
            }
            Some("--workspace") => {
                workspace = Some(PathBuf::from(values.next().ok_or("--workspace requires a path")?));
            }
            Some("--help") | Some("-h") => {
                println!(
                    "usage: cargo run -p tea-agent --example zen-free-verification --features live-verification -- --catalog-evidence PATH --ledger PATH --out PATH [--run-counterparts] [--live --synthetic-or-public-fixtures --tea-home PATH --workspace PATH]"
                );
                std::process::exit(0);
            }
            _ => return Err(format!("unknown verification argument: {}", argument.to_string_lossy())),
        }
    }
    Ok(Arguments {
        catalog_evidence: catalog_evidence.ok_or("--catalog-evidence is required")?,
        ledger: ledger.ok_or("--ledger is required")?,
        out: out.ok_or("--out is required")?,
        live,
        synthetic_or_public_fixtures,
        run_counterparts,
        tea_home,
        workspace,
    })
}

fn write_report(path: &std::path::Path, report: &JsonValue) -> Result<(), String> {
    if path.exists() {
        return Err(format!("refusing to overwrite existing report {}", path.display()));
    }
    let parent = path.parent().ok_or("--out must have an explicit parent directory")?;
    if !parent.is_dir() {
        return Err(format!("report parent is not a directory: {}", parent.display()));
    }
    let content = report
        .to_json_string_pretty()
        .map_err(|error| format!("cannot encode report: {error}"))?;
    fs::write(path, format!("{content}\n"))
        .map_err(|error| format!("cannot write sanitized report {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mandatory_counterparts_are_exactly_six_and_provider_free() {
        let cases = verification_cases();
        assert_eq!(cases.len(), 6);
        assert_eq!(
            cases.iter().map(|case| case.id).collect::<Vec<_>>(),
            vec![
                "synthetic-coding",
                "headless-and-one-shot",
                "interrupted-reopen",
                "forced-compaction",
                "luau-activation-rollback",
                "isolated-children",
            ]
        );
        assert!(
            cases
                .iter()
                .all(|case| case.counterpart.first() == Some(&"cargo"))
        );
        assert_eq!(cases[3].consumer, VerificationConsumer::Compaction);
        assert_eq!(cases[4].consumer, VerificationConsumer::CandidateEvaluation);
        assert_eq!(cases[5].consumer, VerificationConsumer::Child);
        assert_eq!(
            fixture_expected("headless-and-one-shot"),
            Some("crates/tea-core/fixtures/expected/single-turn-text.json")
        );
        assert_eq!(
            fixture_expected("interrupted-reopen"),
            Some("crates/tea-core/fixtures/expected/model-stream-cancellation-reuse.json")
        );
    }
}
