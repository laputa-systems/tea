use tea_core::hooks::BeforeToolCall;
use tea_core::state::{SerializedJson, ToolCallId};
use tea_core::tool::ToolCall;
use tea_luau::{LuaPolicy, PolicyError, PolicyLimits};

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(format!("hardening-{name}")).expect("test IDs are non-empty"),
        name: name.to_owned(),
        arguments: SerializedJson::new("{}"),
    }
}

#[test]
fn ambient_process_file_module_and_debug_globals_are_not_available() {
    let policy = LuaPolicy::load(
        r#"
            local require_ok = pcall(function() return require("tea_core_missing") end)
            local loadfile_ok = pcall(function()
                return loadfile("/__tea_core_missing_file__")
            end)
            local dofile_ok = pcall(function()
                return dofile("/__tea_core_missing_file__")
            end)
            return {
                prompt_sections = {
                    {
                        id = "ambient",
                        content = "io=" .. tostring(io == nil)
                            .. ",os=" .. tostring(os == nil)
                            .. ",package=" .. tostring(package == nil)
                            .. ",debug=" .. tostring(debug == nil)
                            .. ",require=" .. tostring(require_ok)
                            .. ",loadfile=" .. tostring(loadfile_ok)
                            .. ",dofile=" .. tostring(dofile_ok),
                    },
                },
            }
        "#,
    )
    .expect("the policy should load without ambient globals");

    assert_eq!(
        policy.prompt_sections()[0].content,
        "io=true,os=true,package=true,debug=true,require=false,loadfile=false,dofile=false"
    );
}

#[test]
fn sandbox_rejects_global_table_mutation() {
    let error = match LuaPolicy::load(
        r#"
            _G.tea_core_hardening_probe = true
            return { prompt_sections = {} }
        "#,
    ) {
        Ok(_) => panic!("a policy must not mutate the VM global table"),
        Err(error) => error,
    };

    assert!(matches!(error, PolicyError::Runtime { .. }));
}

#[test]
fn zero_resource_limits_are_rejected_by_field() {
    let cases = [
        (
            "max_source_bytes",
            PolicyLimits {
                max_source_bytes: 0,
                ..PolicyLimits::default()
            },
        ),
        (
            "max_memory_bytes",
            PolicyLimits {
                max_memory_bytes: 0,
                ..PolicyLimits::default()
            },
        ),
        (
            "max_interrupt_checks",
            PolicyLimits {
                max_interrupt_checks: 0,
                ..PolicyLimits::default()
            },
        ),
    ];

    for (field, limits) in cases {
        let error = match LuaPolicy::load_with_limits("return { prompt_sections = {} }", limits) {
            Ok(_) => panic!("zero {field} must be rejected before VM setup"),
            Err(error) => error,
        };
        assert_eq!(error, PolicyError::InvalidLimit { field });
    }
}

#[test]
fn source_limit_is_a_deterministic_byte_boundary() {
    let source = "return { prompt_sections = {} }";
    let exact = PolicyLimits {
        max_source_bytes: source.len(),
        ..PolicyLimits::default()
    };
    LuaPolicy::load_with_limits(source, exact).expect("source at the byte limit should load");

    let below = PolicyLimits {
        max_source_bytes: source.len() - 1,
        ..PolicyLimits::default()
    };
    assert!(matches!(
        LuaPolicy::load_with_limits(source, below),
        Err(PolicyError::SourceTooLarge { .. })
    ));
}

#[test]
fn allocation_budget_terminates_a_policy_during_initial_evaluation() {
    let error = match LuaPolicy::load_with_limits(
        r#"
            local values = {}
            for index = 1, 100000 do
                values[index] = string.rep("allocation", 32)
            end
            return { prompt_sections = { { id = "allocation", content = tostring(#values) } } }
        "#,
        PolicyLimits {
            max_source_bytes: 4 * 1024,
            max_memory_bytes: 16 * 1024,
            max_interrupt_checks: 10_000,
        },
    ) {
        Ok(_) => panic!("an unbounded allocation must hit the VM memory limit"),
        Err(error) => error,
    };

    assert!(matches!(error, PolicyError::Runtime { .. }));
}

#[test]
fn interrupt_budget_terminates_a_malicious_loop_hook() {
    let policy = LuaPolicy::load_with_limits(
        r#"
            return {
                prompt_sections = {},
                before_tool = function(_) while true do end end,
            }
        "#,
        PolicyLimits {
            max_source_bytes: 4 * 1024,
            max_memory_bytes: 1024 * 1024,
            max_interrupt_checks: 2,
        },
    )
    .expect("the loop is evaluated only when the hook runs");

    assert!(matches!(
        policy.before_tool_call(&call("loop")),
        Err(PolicyError::Runtime { .. })
    ));
}

#[test]
fn recursion_budget_terminates_a_malicious_hook() {
    let policy = LuaPolicy::load_with_limits(
        r#"
            local function recurse()
                return recurse()
            end
            return {
                prompt_sections = {},
                before_tool = function(_) return recurse() end,
            }
        "#,
        PolicyLimits {
            max_source_bytes: 4 * 1024,
            max_memory_bytes: 1024 * 1024,
            max_interrupt_checks: 4,
        },
    )
    .expect("recursive code is evaluated only when the hook runs");

    assert!(matches!(
        policy.before_tool_call(&call("recursive")),
        Err(PolicyError::Runtime { .. })
    ));
}

#[test]
fn policy_error_does_not_poison_or_permanently_disable_the_policy() {
    let policy = LuaPolicy::load(
        r#"
            local first_call = true
            return {
                prompt_sections = {},
                before_tool = function(_)
                    if first_call then
                        first_call = false
                        error("intentional policy failure")
                    end
                    return "allow"
                end,
            }
        "#,
    )
    .expect("policy should load before its hook is invoked");

    assert!(matches!(
        policy.before_tool_call(&call("first")),
        Err(PolicyError::Runtime { .. })
    ));
    assert_eq!(
        policy.before_tool_call(&call("second")),
        Ok(BeforeToolCall::Allow)
    );
}

#[test]
fn policies_have_isolated_vm_state_when_interleaved() {
    let left = LuaPolicy::load(
        r#"
            local count = 0
            return {
                prompt_sections = { { id = "left", content = "left" } },
                before_tool = function(_)
                    count = count + 1
                    return { action = "block", reason = "left-" .. tostring(count) }
                end,
            }
        "#,
    )
    .expect("left policy should load");
    let right = LuaPolicy::load(
        r#"
            local count = 0
            return {
                prompt_sections = { { id = "right", content = "right" } },
                before_tool = function(_)
                    count = count + 1
                    return { action = "block", reason = "right-" .. tostring(count) }
                end,
            }
        "#,
    )
    .expect("right policy should load");

    assert_eq!(left.prompt_sections()[0].content, "left");
    assert_eq!(right.prompt_sections()[0].content, "right");
    assert_eq!(
        left.before_tool_call(&call("left-1")),
        Ok(BeforeToolCall::Block {
            reason: "left-1".to_owned(),
        })
    );
    assert_eq!(
        right.before_tool_call(&call("right-1")),
        Ok(BeforeToolCall::Block {
            reason: "right-1".to_owned(),
        })
    );
    assert_eq!(
        left.before_tool_call(&call("left-2")),
        Ok(BeforeToolCall::Block {
            reason: "left-2".to_owned(),
        })
    );
    assert_eq!(
        right.before_tool_call(&call("right-2")),
        Ok(BeforeToolCall::Block {
            reason: "right-2".to_owned(),
        })
    );
}

#[test]
fn equivalent_policy_declarations_are_deterministic() {
    let source = r#"
        return {
            prompt_sections = { { id = "stable", content = "stable prompt" } },
            tools = {
                {
                    name = "first",
                    description = "First tool",
                    capability = "world.read",
                    execution_mode = "sequential",
                    schema_json = '{"type":"object","properties":{"x":{"type":"string"}}}',
                },
                {
                    name = "second",
                    description = "Second tool",
                    capability = "world.write",
                    execution_mode = "parallel",
                    schema_json = '{"type":"object"}',
                },
            },
            before_tool = function(call)
                return call.name == "first" and "allow"
                    or { action = "terminate", reason = "stable decision" }
            end,
        }
    "#;
    let first = LuaPolicy::load(source).expect("first equivalent policy should load");
    let second = LuaPolicy::load(source).expect("second equivalent policy should load");

    assert_eq!(first.prompt_sections(), second.prompt_sections());
    assert_eq!(first.tools(), second.tools());
    assert_eq!(
        first.before_tool_call(&call("first")),
        second.before_tool_call(&call("first"))
    );
    assert_eq!(
        first.before_tool_call(&call("other")),
        second.before_tool_call(&call("other"))
    );
}

#[test]
fn invalid_tool_cancellation_settlement_is_rejected() {
    let error = match LuaPolicy::load(
        r#"
            return {
                prompt_sections = {},
                tools = {
                    {
                        name = "transaction",
                        description = "Commit one transaction.",
                        capability = "world.write",
                        execution_mode = "parallel",
                        cancellation_settlement_mode = "unknown",
                        schema_json = '{"type":"object"}',
                    },
                },
            }
        "#,
    ) {
        Ok(_) => panic!("unknown cancellation settlement must fail closed"),
        Err(error) => error,
    };

    assert!(
        matches!(error, PolicyError::Contract { message } if message.contains("cancellation_settlement_mode"))
    );
}
