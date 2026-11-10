//! Definitions of the executor-injected tools (request_user_input,
//! update_goal, request_permissions, spawn_agent) advertised to the model.
//!
//! `ask_user` has no definition: it is a parser alias only, see
//! [`is_user_input_tool`].

use leveler_model::{ToolCall, ToolDefinition};

/// Primary name for mid-turn clarification.
pub(crate) const REQUEST_USER_INPUT_TOOL: &str = "request_user_input";

/// Legacy alias kept for older prompts / transcripts; same Clarifier path.
pub(crate) const ASK_USER_TOOL: &str = "ask_user";

/// Shared schema for `request_user_input` and `ask_user`.
///
/// `questions` is the multi-question form: one interaction the user answers
/// as a set, shown as tabs. `question`/`options` remain the single-question
/// form so a model that asks one thing keeps working unchanged.
fn user_input_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "question": {
                "type": "string",
                "description": "The question to ask. With `questions`, use it as the interaction's one-line headline."
            },
            "options": {
                "type": "array",
                "items": { "type": "string" },
                "description": "2-4 mutually exclusive choices, one short line each. Omit for a free-text answer."
            },
            "questions": {
                "type": "array",
                "description": "Several questions answered in one interaction, one tab each.",
                "items": {
                    "type": "object",
                    "properties": {
                        "header": {
                            "type": "string",
                            "description": "Short tab label (2-6 characters), e.g. 数据策略."
                        },
                        "question": {
                            "type": "string",
                            "description": "The full question, one short sentence."
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["single", "multi", "text"],
                            "description": "single = exactly one of `options`; multi = any number of `options`; text = the user types the answer."
                        },
                        "options": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "2-4 mutually exclusive choices for single or multi, one short line each."
                        },
                        "allow_other": {
                            "type": "boolean",
                            "description": "Adds a free-text entry after the options."
                        },
                        "min_choices": {
                            "type": "integer",
                            "description": "multi only: fewest picks the user must make (default 0)."
                        },
                        "max_choices": {
                            "type": "integer",
                            "description": "multi only: most picks the user may make (default: all options)."
                        }
                    },
                    "required": ["question", "kind"]
                }
            }
        },
        "required": []
    })
}

fn user_input_description() -> String {
    format!(
        "Ask the user a question and wait for the answer (`{REQUEST_USER_INPUT_TOOL}`). \
         The turn pauses until the user responds or the request is cancelled. \
         Assistant prose does not pause the turn.\n\n\
         Single question: `question`, plus optional `options` (2–4 mutually \
         exclusive choices, one short line each). Omit `options` for a free-text \
         answer such as a credential, a name, or a path.\n\n\
         Several questions in one interaction: `questions`. Each item requires \
         `question` and `kind` (`single`, `multi`, or `text`). `single` and \
         `multi` take `options`. Optional fields: `header` (short tab label), \
         `allow_other`, `min_choices`, `max_choices`. Optional `question` on the \
         call is the interaction's one-line headline."
    )
}

/// Whether `name` is a clarification tool (primary or legacy).
pub(crate) fn is_user_input_tool(name: &str) -> bool {
    name == REQUEST_USER_INPUT_TOOL || name == ASK_USER_TOOL
}

/// Primary clarification tool definition.
///
/// The legacy `ask_user` name is a parser alias only
/// ([`is_user_input_tool`]): the model-visible surface never advertises it, so
/// no definition is built for it.
pub(crate) fn request_user_input_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: REQUEST_USER_INPUT_TOOL.to_string(),
        description: user_input_description(),
        input_schema: user_input_input_schema(),
    }
}

/// The name of the injected goal-resolution tool (goal mode only). The run does
/// not end when the model goes quiet — it ends only when the model calls this to
/// mark the objective `complete` (done and truthfully reported) or `blocked`
/// (truly stuck).
pub(crate) const UPDATE_GOAL_TOOL: &str = "update_goal";

/// Resolve the current objective. Verification is evidence, not a universal
/// completion gate: the description asks for a truthful claim about the
/// current workspace state, not for a named proof tool. Completion semantics,
/// the objective-as-stated rule, and the tool's shape are unchanged. A plan is
/// a declaration the model may update freely; the description does not require
/// a terminal Plan refresh.
pub(crate) fn update_goal_tool_definition() -> ToolDefinition {
    let description = String::from(
        "Resolve the current objective. Call this ONLY to end the \
            task: `complete` when every requirement is done, against the \
            current workspace state; \
            `blocked` when you are genuinely and \
            repeatedly stuck and cannot make progress. Going silent does NOT end \
            the task — you must call this. Do not mark complete on unproven or \
            indirect evidence, and do not redefine success down to what already \
            exists. `complete` means the objective AS THE USER STATED IT. If the \
            stated objective cannot be satisfied — it conflicts with something \
            you must not change (an existing test that pins the opposite \
            behavior, a frozen interface, an explicit prohibition) — do NOT \
            reinterpret or narrow its terms into a weaker task and complete \
            that instead: that is a false completion. Report `blocked`, name \
            the exact conflict (which requirement collides with which \
            constraint), and first revert edits that served only the abandoned \
            attempt so the workspace is left clean. Partial conflicts are the \
            same rule: when only PART of the stated objective is satisfiable, \
            delivering that part while quietly exempting the rest — with or \
            without a rationalization — is still a false completion. Blocked, \
            naming which part cannot be satisfied and why. \
            `complete` needs `verification`: `passed` citing the executions \
            you rely on, one per line in `detail` (each citation is checked \
            against the runtime's execution record, after your last change), or \
            `not_applicable` with the reason in `detail`. The runtime never \
            classifies a command by its name: the citation is what makes an \
            execution evidence, and a completion that changed the workspace \
            with no citation is accepted as unverified, never as a verified \
            success. When the goal has a \
            recorded requirement set, `requirements` must account for every \
            active deliverable by its stable id; a missing or `not_done` \
            requirement refuses completion, and the requirement's own account \
            is the evidence the terminal carries. A `tests` requirement names \
            its exact test command in `target`; a passing recorded run of that \
            command accounts for it, but command success alone is reported as \
            unverified test coverage — the runtime has no structured test \
            result to confirm that any test executed. \
            `unverifiable` marks delivered work the runtime cannot prove, not \
            work you skipped.",
    );
    ToolDefinition {
        name: UPDATE_GOAL_TOOL.to_string(),
        description,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["complete", "blocked"],
                    "description": "complete = done, for the objective as stated; blocked = truly stuck, or the stated objective cannot be satisfied as written (say what conflicts with what)."
                },
                "summary": {
                    "type": "string",
                    "description": "Concise goal summary shown by the TUI as a recap. Keep ≤12 words for complete; longer only when blocked. Do not restate the user question or list files you read."
                },
                "deliver_answer": {
                    "type": "boolean",
                    "description": "Set true when the visible text you wrote in this SAME response is your complete final answer to the user, ready to be delivered verbatim. It may be the text of this response, or the last text you wrote before a bookkeeping-only round (`update_plan`) with no work tool in between. Omit it when the text only announces a next step or reports progress. Once you have offered an answer, setting this again does not replace it. A citation repair does not replace an answer already written: cite `call_id:<id>` or the exact command, and leave the answer text as it stands."
                },
                "revise_answer": {
                    "type": "boolean",
                    "description": "Set true only when the visible text of this response REPLACES the answer you already offered because new evidence changed your conclusion. It implies `deliver_answer`. Omit it when you are only repairing verification citations or reporting progress for an answer that is still correct; the answer already written remains the delivery."
                },
                "next_step": {
                    "type": "string",
                    "description": "Optional concrete follow-up for the user. Omit this field when no genuine next step remains; never copy or paraphrase the conversation merely to fill it."
                },
                "verification": {
                    "type": "object",
                    "description": "For complete: the executions you cite as evidence. The runtime checks every citation against its own record; it does not classify commands by name.",
                    "properties": {
                        "status": {
                            "type": "string",
                            "enum": ["passed", "not_applicable"]
                        },
                        "detail": {
                            "type": "string",
                            "description": "For passed: cite each execution you rely on, one per line. Prefer `call_id:<id>` of the tool call that ran it. A full command line also matches; its name is not classified. A trailing result note (`→ ...` or a parenthetical) is not part of the command. Do not abbreviate a command, and do not rerun a check only to make the citation match. For not_applicable: why no executable check exists."
                        }
                    },
                    "required": ["status", "detail"]
                },
                "requirements": {
                    "type": "array",
                    // No fixed `maxItems`: the cap is the number of active
                    // deliverables in THIS goal, which the schema cannot know
                    // when the tool is defined. `parse_accounting` enforces the
                    // real cap and names it in the refusal.
                    "description": "For complete: one entry per active deliverable id in the goal's recorded requirement set (`get_goal_scope` lists the ids). Keyed by id, not by restating the requirement text.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "The requirement's stable id, for example \"R1\"."
                            },
                            "status": {
                                "type": "string",
                                "enum": ["done", "not_done", "unverifiable"]
                            },
                            "evidence": {
                                "type": "string",
                                "description": "What proves it. For a `tests` requirement, put `call_id:<id>` of a passing run of the declared test command (`target`), or that exact command, on its own line."
                            }
                        },
                        "required": ["id", "status", "evidence"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["status", "summary"]
        }),
    }
}

/// The tool result the model reads when a Goal resolution was accepted.
///
/// An accepted completion can still carry boundaries the runtime could not
/// verify: a requirement the user stated that no command, file or record can
/// prove. Those boundaries are already shown to the user, and before this they
/// existed only there — the model wrote the delivery without them and could
/// claim more than the record held. This states the runtime's own facts on the
/// acceptance, which is not extra work and costs nothing when there is no
/// boundary.
pub(crate) fn completion_receipt(resolution: &str, warnings: &[String]) -> String {
    let mut receipt = format!("{resolution}.");
    if warnings.is_empty() {
        return receipt;
    }
    receipt.push_str(
        "\nUnverified boundaries of this completion — the runtime could not check these \
         mechanically, so they rest on your account, not on evidence:",
    );
    for warning in warnings.iter().take(6) {
        receipt.push_str("\n- ");
        receipt.push_str(warning);
    }
    if warnings.len() > 6 {
        receipt.push_str(&format!("\n- …and {} more", warnings.len() - 6));
    }
    receipt.push_str(
        "\nYour answer must not present these as verified. Say which parts you verified and \
         which parts you did not.",
    );
    receipt
}

/// The name of the injected task-commitment tool (Chat turns only).
///
/// This is the goal-commitment entry, not a delegation entry: the model
/// declares that the user's request is work to be carried to completion, and
/// the running turn binds itself to a durable goal. No sub-agent is created —
/// `spawn_agent` is the only tool that delegates. It is not inferred from the
/// user's words.
pub const COMMIT_TASK_TOOL: &str = "commit_task";

/// The structured `GoalIntercepted` kind the executor emits when it refuses a
/// `commit_task` declaration. The declaration is what leaves the work
/// unregistered, whatever field the refusal names.
///
/// This is the durable registration fact: clients must not infer that state
/// from the terminal's warning list — a plain Chat answer can carry an
/// unrelated review warning without ever attempting a declaration. This wire
/// value is retained for replay of records written before
/// [`COMMIT_INVALID_DECLARATION_KIND`] existed; new refusals use the accurate
/// kind below.
pub const COMMIT_REFUSED_KIND: &str = "acceptance_narrowed";

/// The structured kind for a `commit_task` whose arguments are not a valid
/// declaration at all: a missing or mistyped field, a value over its limit, a
/// requirement that cannot be anchored, or an acceptance check that does not
/// exist. Naming this "acceptance_narrowed" stated a cause the refusal never
/// established. The gate itself is unchanged — the declaration is still
/// refused and the work remains unregistered.
pub const COMMIT_INVALID_DECLARATION_KIND: &str = "invalid_declaration";

/// Whether this `GoalIntercepted` kind is a refused `commit_task` declaration.
/// Both values mean the same durable fact; one is the legacy wire value.
pub fn is_commit_refusal_kind(kind: &str) -> bool {
    kind == COMMIT_REFUSED_KIND || kind == COMMIT_INVALID_DECLARATION_KIND
}

/// Commit the current Chat turn to the Goal lifecycle.
///
/// The description carries the decision rule because it is the only place the
/// model is told when a request stops being a question and becomes delegated
/// work. The runtime never classifies the user's text for it: no keyword,
/// length, tool count or round count decides this — the model reads the
/// request's meaning and expected result.
/// The name of the read-only effective-scope tool.
pub(crate) const GET_GOAL_SCOPE_TOOL: &str = "get_goal_scope";

/// Read-only projection of the goal's recorded requirement set and history.
pub(crate) fn get_goal_scope_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: GET_GOAL_SCOPE_TOOL.to_string(),
        description: "Read this goal's recorded requirement set and, on request, its ordered \
            change history. Every requirement carries a stable id, kind, state, the verbatim \
            user span it was declared from, and the acceptance check the model associated with \
            it. The runtime never re-derives this set from prose, so this is the exact set the \
            completion gate reads. Read-only; it never registers or changes a goal."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Zero-based index of the first requirement to return (default 0)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "description": "Maximum requirements (and history events) to return (default 20)."
                },
                "history": {
                    "type": "boolean",
                    "description": "Also list the ordered requirement-change events with the turn that authorized each one."
                }
            },
            "additionalProperties": false
        }),
    }
}

/// The structured requirement-scope tool.
pub(crate) const DECLARE_SCOPE_TOOL: &str = "declare_scope";

/// The `kind` enum the model sees, from the runtime's own kind list.
fn requirement_kind_enum() -> serde_json::Value {
    serde_json::json!(
        leveler_lifecycle::RequirementKind::ALL
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>()
    )
}

/// The conditional `target` contract, generated from the runtime's own
/// `requires_target` truth. A schema cannot conditionally require a field
/// without an `if`/`then` construct that not every provider accepts, so the
/// contract is stated in the field descriptions and enforced by validation.
/// Generating the kind list keeps the model's copy and the runtime from
/// drifting apart.
fn requirement_target_contract() -> String {
    let kinds = leveler_lifecycle::RequirementKind::ALL
        .iter()
        .filter(|kind| kind.requires_target())
        .map(|kind| format!("`{}`", kind.as_str()))
        .collect::<Vec<_>>();
    format!(
        "{} need `target` (path, file or exact command).",
        kinds.join(", ")
    )
}

/// Declare what the user's newest message changes about the goal's
/// requirements.
///
/// The runtime validates that each operation's `source_quote` occurs in the
/// newest user input, that referenced requirement ids exist in this goal, and
/// that the operation sequence is legal over the recorded history. It never
/// interprets the text: the model supplies the reading, the runtime supplies
/// identity, order and durability.
pub(crate) fn declare_scope_tool_definition() -> ToolDefinition {
    let description = String::from(
        "Change this goal's recorded requirements when the user's newest message adds, \
            amends, replaces, waives or restores something. `source_quote` must copy the part \
            of the user's newest message that carries the change — the runtime checks that it \
            really occurs there, so never paraphrase or invent one. Use requirement ids from \
            commit_task or get_goal_scope. `add` mints a new id and needs `kind` and `text`; \
            `amend` changes fields of an active requirement; `replace` retires an active \
            requirement and mints a new id carrying the replacement; `waive` marks an active \
            requirement no longer owed without deleting its history; `restore` revives a \
            waived requirement. Constraints use the same operations as deliverables. Do not \
            waive or replace an unrelated requirement the user did not mention: every \
            operation must quote the user's own words for that change.",
    );
    let kind_enum = requirement_kind_enum();
    let target_contract = requirement_target_contract();
    ToolDefinition {
        name: DECLARE_SCOPE_TOOL.to_string(),
        description,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "operations": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 8,
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": {
                                "type": "string",
                                "enum": ["add", "amend", "replace", "waive", "restore"]
                            },
                            "requirement_id": {
                                "type": "string",
                                "description": "The existing requirement this operation targets. Required for amend, replace, waive and restore."
                            },
                            "kind": {
                                "type": "string",
                                "enum": kind_enum,
                                "description": format!(
                                    "Required for add and replace; optional for amend. {target_contract}"
                                )
                            },
                            "text": {
                                "type": "string",
                                "description": "For `content`, the exact line the target must contain — not a description of it. Otherwise the requirement in the user's terms. Required for add and replace; optional for amend."
                            },
                            "target": {
                                "type": "string",
                                "description": format!(
                                    "The path, file or exact command named by `kind`. For `amend`, set it when changing `kind` to one of these. {target_contract}"
                                )
                            },
                            "source_quote": {
                                "type": "string",
                                "description": "Verbatim span of the user's newest message that states this change. Whitespace differences are tolerated; paraphrases are not."
                            },
                            "check": {
                                "type": "string",
                                "description": "An acceptance check that covers this requirement, copied exactly from the goal's acceptance list. Omit when none covers it."
                            },
                            "reason": {
                                "type": "string",
                                "description": "Optional short note, especially for waive or replace: why the requirement changed."
                            }
                        },
                        "required": ["op", "source_quote"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["operations"],
            "additionalProperties": false
        }),
    }
}

/// The name of the structured user-intent tool.
pub(crate) const DECLARE_INTENT_TOOL: &str = "declare_intent";

/// Translate the user's newest message into the lifecycle transition it means.
///
/// This is the only entry point through which a user message can continue,
/// pause, re-bound, amend or cancel a goal. The runtime never reads the
/// message's words: it validates that `source_quote` really occurs in the
/// newest user input, that every named plan step exists in the model's own
/// declared plan, and that the operation sequence is a legal transition over
/// the durable goal state. Every language, length and phrasing is equally
/// valid — the model supplies the meaning, the runtime supplies the mechanics.
pub(crate) fn declare_intent_tool_definition() -> ToolDefinition {
    let description = String::from(
        "State what the user's newest message means for the task lifecycle. Call this \
            BEFORE continuing, pausing, changing or cancelling an existing goal, from any \
            language or phrasing. `source_quote` must copy the part of the user's newest \
            message that carries the instruction — the runtime checks that it really occurs \
            there, so never paraphrase or invent one. Put the operations in the order the \
            user meant them: `continue_task` releases a pause, `pause_task` stops at a \
            boundary, `set_pause_boundary` / `replace_pause_boundary` move a future boundary \
            without releasing a current pause, `amend_current_task` records a refinement of \
            the same goal, `cancel_task` ends it, `start_independent_task` says the request is \
            unrelated to any running goal, `answer_only` says the user asked a question and \
            no goal drive is authorized. Compose them for a compound instruction such as \
            \"continue, but pause after stage three\": continue_task, then \
            set_pause_boundary. A boundary names a `step_id` from your own plan, so call \
            update_plan with stable `id` values first. Only a declaration of this tool can \
            release a pause or cancel a goal; your own prose cannot. If the user's message \
            does not change the lifecycle, do not call this.",
    );
    ToolDefinition {
        name: DECLARE_INTENT_TOOL.to_string(),
        description,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "source_quote": {
                    "type": "string",
                    "description": "Verbatim span of the user's newest message that this declaration interprets. Whitespace differences are tolerated; paraphrases are not."
                },
                "goal_id": {
                    "type": "string",
                    "description": "The running goal this applies to. Omit when exactly one running goal exists for this task; the runtime refuses to guess among several."
                },
                "operations": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 8,
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": {
                                "type": "string",
                                "enum": [
                                    "continue_task",
                                    "pause_task",
                                    "set_pause_boundary",
                                    "replace_pause_boundary",
                                    "amend_current_task",
                                    "start_independent_task",
                                    "cancel_task",
                                    "answer_only"
                                ]
                            },
                            "boundary": {
                                "type": "object",
                                "description": "Required for pause_task, set_pause_boundary and replace_pause_boundary.",
                                "properties": {
                                    "kind": {
                                        "type": "string",
                                        "enum": ["now", "after_any_step", "after_step"]
                                    },
                                    "step": {
                                        "type": "object",
                                        "description": "Required when kind is after_step.",
                                        "properties": {
                                            "step_id": {"type": "string"},
                                            "label": {"type": "string"}
                                        },
                                        "required": ["step_id"],
                                        "additionalProperties": false
                                    }
                                },
                                "required": ["kind"],
                                "additionalProperties": false
                            }
                        },
                        "required": ["kind"],
                        "additionalProperties": false
                    },
                    "description": "Ordered operations. Applied in this order."
                }
            },
            "required": ["source_quote", "operations"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn commit_task_tool_definition() -> ToolDefinition {
    let description = String::from(
        "Declare that the user's request is execution work this turn must carry \
            to completion, and commit the turn to the task lifecycle. Call it before the \
            first edit only when ALL THREE hold: (1) the user asked you to perform work, \
            not to explain, discuss, check or confirm something; (2) the request states a \
            concrete result you can deliver (files, modules, endpoints, stages, migrations); \
            (3) finishing it needs several steps, tracking or acceptance, so a stage summary \
            would not prove it done — or the user said to finish everything before stopping. \
            Call it even when you expect to finish this turn: the commitment is what keeps it \
            alive if the work outlasts it. Do NOT call it for a question, an explanation, a status \
            or verification check, a discussion, a single small fix, or when the user asked you \
            not to modify anything. If you are unsure whether this is execution work, do not \
            call it. `objective` is the user's stated end state, neither narrower nor larger; \
            when the user's message itself is non-empty the runtime records it as the durable \
            objective, so `objective` is unused. \
            `acceptance` lists the concrete checks that will prove it. If the user stated \
            explicit deliverables or constraints, declare them in `requirements`, one entry \
            each, with a verbatim `source_quote` from the user's message: that structured set \
            is what the completion gate reads, and the runtime never re-derives it from prose. \
            After the commit, the \
            runtime keeps this turn running: a stage summary or a quiet end does NOT finish the \
            task. Continue the work and resolve it with update_goal(complete|blocked). Do not \
            substitute update_plan for the acceptance contract: a plan records intended steps, \
            not proof that the user's deliverables exist. This is not a Git commit: it records \
            an execution task in CodeLeveler and does not stage, commit or alter repository \
            files. The session itself stays a Chat session.",
    );
    let kind_enum = requirement_kind_enum();
    let target_contract = requirement_target_contract();
    ToolDefinition {
        name: COMMIT_TASK_TOOL.to_string(),
        description,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "The user's requested end state, as stated. Do not add, narrow or rewrite requirements."
                },
                "acceptance": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 1,
                    "maxItems": 20,
                    "description": "The model's concrete checks. Cover every deliverable; one check may name several related deliverables. A plan, not the user's scope."
                },
                "requirements": {
                    "type": "array",
                    "maxItems": 40,
                    "description": "The user's stated deliverables and constraints, one entry each with a verbatim quote. Omit if none beyond the objective.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": {
                                "type": "string",
                                "enum": kind_enum,
                                "description": format!(
                                    "artifact = must exist; artifact_absent = must be gone; \
                                     command = exact command; named = no mechanical check; \
                                     content = literal in target; tests = suite run by `target`; \
                                     others not owed. {target_contract}"
                                )
                            },
                            "text": {
                                "type": "string",
                                "description": "For `content`, the exact line the target must contain — not a description of it. Otherwise the requirement in the user's terms."
                            },
                            "target": {
                                "type": "string",
                                "description": "The path, file or exact command named by `kind`."
                            },
                            "source_quote": {
                                "type": "string",
                                "description": "Verbatim span of the user's message that states it; the runtime checks it occurs there."
                            },
                            "check": {
                                "type": "string",
                                "description": "One `acceptance` entry covering it, copied exactly."
                            }
                        },
                        "required": ["kind", "text", "source_quote"],
                        "additionalProperties": false
                    }
                },
                "task_relation": {
                    "type": "string",
                    "enum": ["continue", "new"],
                    "description": "Required when this task already owes a running goal and the objective differs: \"continue\" joins that goal (the user is extending the same work), \"new\" starts an independent task with its own objective, acceptance and budget. Omit only when no goal is running or the objective is identical; the runtime refuses to guess."
                }
            },
            "required": ["objective", "acceptance"],
            "additionalProperties": false
        }),
    }
}

/// The Chat commitment rule. It is a product contract, not a classifier:
/// the model declares the commitment with `commit_task`, and every behavior
/// change follows that durable fact. Kept as a constant so its prompt cost is
/// measured, not guessed.
///
/// It is named COMMITMENT, not delegation, because the two are different
/// actions: committing keeps this turn working toward a goal, while delegation
/// (`spawn_agent`) starts another agent. One word for both made the model's own
/// summary of its work claim it had delegated a task it had only committed.
pub(crate) const TASK_COMMIT_GUIDANCE: &str = "TASK COMMITMENT: if the user asked you to carry out work to \
     completion and the request states a concrete deliverable that needs several steps, tracking or \
     acceptance (or the user said to finish everything before stopping), call `commit_task` before the \
     first edit — even if you expect to finish this turn; the commitment keeps the turn alive if the \
     work outlasts it. Use the objective as the user stated it and the checks that will prove it. If \
     you are already mid-task and about to end while requested work is unfinished, call `commit_task` \
     instead of summarizing what remains. Do not call it for a question, an explanation, a status or \
     verification check, a discussion, a single small fix, or when the user asked you not to modify \
     anything; if unsure, do not call it. Once committed, the runtime keeps the turn running until \
     update_goal resolves the task.";

/// The name of the injected request-permissions tool.
pub(crate) const REQUEST_PERMISSIONS_TOOL: &str = "request_permissions";

/// The name of the injected sub-agent spawn tool.
pub(crate) const SPAWN_AGENT_TOOL: &str = "spawn_agent";

/// The tool the model calls to run a focused sub-agent on a self-contained
/// subtask, getting back only its final result. Emitting several calls in one
/// turn runs the sub-agents CONCURRENTLY.
pub(crate) fn spawn_agent_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: SPAWN_AGENT_TOOL.to_string(),
        description: "Start a child agent on a subtask. The child uses this model \
            and workspace and starts a new conversation: it does not see this one. \
            `task` is the child's whole instruction. `title` is the short display \
            name and stays fixed for that child's lifetime.\n\n\
            Calls in one assistant message run concurrently. Calls in a later \
            message run after those return. `run_in_background` defaults to true: \
            the call returns the child's id immediately, and the runtime reports \
            the child's result, including partial work, when the child settles. \
            false waits for that result before this call returns.\n\n\
            The child can read immediately. A write requires claim_write_scope. \
            The ownership fence denies overlapping claims and refuses this \
            agent's writes, by editor or by shell, to a path a running child owns.\n\n\
            `agent` names a declarative agent (project `.leveler/agents/<name>/`, \
            user-level, or built-in). That definition fixes capability, tools, \
            write bounds, model, and instructions. A different `role` or `profile` \
            on the same call is refused. `profile` and `role` are aliases: \
            explorer is read-only, worker is a scoped writer and requires `files`, \
            default claims its own write scope. Reviewer cannot be requested. An \
            unknown agent or role is refused."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Short display name for the delegated task, roughly 3–8 words. Not a copy of `task`. Fixed for the child's lifetime."
                },
                "task": { "type": "string", "description": "The complete, self-contained instruction for the sub-agent." },
                "profile": {
                    "type": "string",
                    "enum": ["default", "explorer", "worker"],
                    "description": "Optional capability contract: explorer = read-only investigation; worker = scoped writer (requires `files`); default = claims its own write scope. Omit for the default child. Reviewer is harness-launched and cannot be requested here. Alias of `role` when both agree; conflicting values are refused."
                },
                "role": {
                    "type": "string",
                    "enum": ["default", "explorer", "worker"],
                    "description": "Optional. Historical alias of `profile`. explorer = read-only investigation; worker = legacy pre-scoped writer (requires `files`); default = normal child that claims its own write scope."
                },
                "files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Legacy, for role='worker' only: pre-claimed exclusive paths. Normal children omit this and claim their own scope after reading the code."
                },
                "agent": {
                    "type": "string",
                    "description": "Name of an available agent (project `.leveler/agents/<name>/`, user-level, or built-in). Its definition sets the capability; an unknown or invalid name is refused."
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Defaults to true: the call returns the child's id immediately and the runtime reports settlement later. false: this call waits for the child's result."
                }
            },
            "required": ["task", "title"]
        }),
    }
}

/// The tool a CHILD calls to acquire exclusive write authority over the paths
/// it actually needs, after reading enough code to know them (late-bound
/// ownership: SPAWN != WRITE AUTHORITY).
pub(crate) const CLAIM_WRITE_SCOPE_TOOL: &str = "claim_write_scope";

pub(crate) fn claim_write_scope_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: CLAIM_WRITE_SCOPE_TOOL.to_string(),
        description: "Claim exclusive write authority over the listed files or \
            directories. Before the first successful claim, mutations are refused. \
            The grant is exclusive and atomic: every path or none. An overlapping \
            claim is denied while another child holds it. The grant is released \
            when this child finishes. A denial does not change the workspace. A \
            later call can add paths."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Relative files or directories to own exclusively (a directory covers its whole subtree, e.g. 'src/output/'). Claim the smallest scope that covers your work — never the repository root."
                }
            },
            "required": ["paths"]
        }),
    }
}

/// Runtime default resolution for `run_in_background`: an omitted parameter
/// means background. The model relies on the advertised default; it does not
/// have to reproduce it on every call.
pub(crate) fn resolve_run_in_background(arguments: &serde_json::Value) -> bool {
    arguments
        .get("run_in_background")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// The tool a CHILD calls to report one typed finding as it is confirmed.
pub(crate) const REPORT_FINDING_TOOL: &str = "report_finding";

pub(crate) fn report_finding_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: REPORT_FINDING_TOOL.to_string(),
        description: "Record one finding. Each call stores one finding and the \
            parent receives the stored findings with this child's result, including \
            when the run stops early.\n\n\
            Arguments: `kind` (required: relevant_file, relevant_symbol, \
            dependency, callsite, risk, test, config, observation, correctness), \
            `summary` (required text), optional `file`, optional `symbol`."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": [
                        "relevant_file", "relevant_symbol", "dependency",
                        "callsite", "risk", "test", "config", "observation",
                        "correctness"
                    ],
                    "description": "What kind of thing this finding is."
                },
                "summary": {
                    "type": "string",
                    "description": "One specific, evidence-backed sentence."
                },
                "file": { "type": "string", "description": "The file it concerns, if any." },
                "symbol": { "type": "string", "description": "The symbol it concerns, if any." }
            },
            "required": ["kind", "summary"]
        }),
    }
}

/// Turn-scoped elevations from an approved `request_permissions` call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TurnPermissionGrants {
    pub network: bool,
    /// Allow writes to the current workspace's Git metadata while preserving
    /// the workspace boundary.
    pub repository_git: bool,
    /// Drop OS write confinement for run_command/shell_command this turn.
    pub unrestricted_fs: bool,
}

impl TurnPermissionGrants {
    pub fn is_empty(self) -> bool {
        !self.network && !self.repository_git && !self.unrestricted_fs
    }

    /// Whether everything `requested` asks for is already granted.
    pub fn covers(self, requested: Self) -> bool {
        (self.network || !requested.network)
            && (self.repository_git || self.unrestricted_fs || !requested.repository_git)
            && (self.unrestricted_fs || !requested.unrestricted_fs)
    }

    pub fn merge(self, other: Self) -> Self {
        Self {
            network: self.network || other.network,
            repository_git: self.repository_git || other.repository_git,
            unrestricted_fs: self.unrestricted_fs || other.unrestricted_fs,
        }
    }
}

/// Parse model arguments for `request_permissions` (current fields plus legacy aliases).
///
/// - `network` (bool): request network for the rest of the turn
/// - `filesystem`: `"workspace"` (default) | `"git"` | `"unrestricted"` — Git
///   permits current-repository metadata writes; unrestricted clears confinement
/// - `full_access` (bool): shorthand for network + unrestricted filesystem
/// - If none of the above are set, defaults to **network only** (legacy behavior)
pub(crate) fn parse_permission_request(
    args: &serde_json::Value,
) -> (String, String, TurnPermissionGrants) {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("(未说明)")
        .to_string();
    let reason = args
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let (mut grants, named) = parse_grant_axes(args);
    if !named {
        // Legacy: only action/reason → network elevation.
        grants.network = true;
    }
    (action, reason, grants)
}

/// The command tools whose calls may carry an `escalate` retry.
const ESCALATABLE_TOOLS: [&str; 2] = ["shell_command", "run_command"];

/// Read the grant axes (`network` / `filesystem` / `full_access`) shared by
/// `request_permissions` and a command's `escalate`. Returns the grants plus
/// whether any axis was actually named — the two callers disagree on what an
/// unnamed axis means, so that judgement stays with them.
fn parse_grant_axes(args: &serde_json::Value) -> (TurnPermissionGrants, bool) {
    let full_access = args
        .get("full_access")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let network = args.get("network").and_then(|v| v.as_bool());
    let filesystem = args
        .get("filesystem")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if full_access {
        return (
            TurnPermissionGrants {
                network: true,
                repository_git: false,
                unrestricted_fs: true,
            },
            true,
        );
    }
    let named = network.is_some() || !filesystem.is_empty();
    (
        TurnPermissionGrants {
            network: network.unwrap_or(false),
            repository_git: filesystem == "git",
            unrestricted_fs: matches!(
                filesystem,
                "unrestricted" | "full" | "full_access" | "danger-full-access"
            ),
        },
        named,
    )
}

/// Whether a tool's calls may carry `escalate`.
pub(crate) fn is_escalatable_tool(name: &str) -> bool {
    ESCALATABLE_TOOLS.contains(&name)
}

/// Parse a command call's `escalate` retry into its reason and grants.
///
/// `None` means the call is an ordinary one. Unlike `request_permissions`,
/// an `escalate` naming no axis yields EMPTY grants rather than the legacy
/// network default: this argument exists to answer a denial the model just
/// saw, so it must name the axis that denial was about. The caller turns
/// empty grants into a refusal.
pub(crate) fn parse_escalation(args: &serde_json::Value) -> Option<(String, TurnPermissionGrants)> {
    let escalate = args.get("escalate")?;
    if !escalate.is_object() {
        return None;
    }
    let reason = escalate
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let (grants, _) = parse_grant_axes(escalate);
    Some((reason, grants))
}

/// The command line an `escalate` prompt shows the user. The command IS the
/// action here — unlike `request_permissions`, the model does not restate it.
pub(crate) fn escalation_action(call: &ToolCall) -> String {
    if let Some(cmd) = call.arguments.get("cmd").and_then(|v| v.as_str()) {
        return cmd.to_string();
    }
    let program = call
        .arguments
        .get("program")
        .and_then(|v| v.as_str())
        .unwrap_or(call.name.as_str());
    let args: Vec<&str> = call
        .arguments
        .get("args")
        .and_then(|v| v.as_array())
        .map(|args| args.iter().filter_map(|arg| arg.as_str()).collect())
        .unwrap_or_default();
    format!("{program} {}", args.join(" ")).trim().to_string()
}

/// An `escalate` that names no axis. Refused without a prompt: there is
/// nothing concrete to ask the user to approve.
pub(crate) fn escalation_missing_axis_message() -> String {
    "escalate named no permission: set `network: true`, `filesystem: \"git\"` or `filesystem: \"unrestricted\"`, \
     or `full_access: true` on it, matching the access the sandbox actually denied. \
     The command was NOT run."
        .to_string()
}

/// The `escalate` property added to each command tool's published schema.
fn escalation_property() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "Retry this EXACT command with elevated permission after the \
            sandbox denied it. The approval prompt this raises is how the user consents \
            — do not ask in prose first, and do not call request_permissions for it. \
            Ground it in a denial you just saw: never escalate speculatively. Ask only for \
            what that denial showed: a network failure needs `network`, not `filesystem`. \
            The user chooses whether the grant covers this call or the rest of the turn. \
            If the user denies, that answer is final.",
        "properties": {
            "reason": {
                "type": "string",
                "description": "One sentence for the user: why this exact command needs the wider access."
            },
            "network": {
                "type": "boolean",
                "description": "This call needs network access."
            },
            "filesystem": {
                "type": "string",
                "enum": ["workspace", "git", "unrestricted"],
                "description": "git = allow current-repository Git metadata writes while keeping workspace confinement; unrestricted = drop workspace confinement. Request the narrowest axis matching the observed denial."
            },
            "full_access": {
                "type": "boolean",
                "description": "Shorthand for network=true and filesystem=unrestricted."
            }
        },
        "required": ["reason"]
    })
}

/// Publish `escalate` on the command tools.
///
/// Mounted only while the profile actually confines something — under
/// 完全访问 there is nothing to escalate to, so the argument would only invite
/// a pointless prompt (same reason `request_permissions` is withheld there).
pub(crate) fn advertise_escalation(tools: &mut [ToolDefinition]) {
    for tool in tools
        .iter_mut()
        .filter(|tool| is_escalatable_tool(&tool.name))
    {
        if let Some(properties) = tool
            .input_schema
            .get_mut("properties")
            .and_then(|p| p.as_object_mut())
        {
            properties.insert("escalate".to_string(), escalation_property());
        }
    }
}

/// How long an approved elevation lasts. `request_permissions` holds for the
/// rest of the turn, and its prompt says so. A command's `escalate` lasts as
/// long as the option the user picks — this call ("仅允许本次") or the turn
/// ("本轮对话内允许") — so its description claims neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrantScope {
    Turn,
    SingleCall,
}

impl GrantScope {
    fn fs_note(self) -> &'static str {
        match self {
            Self::Turn => "本轮写不受工作区沙箱限制",
            Self::SingleCall => "写不受工作区沙箱限制",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Turn => "本轮需要",
            Self::SingleCall => "需要",
        }
    }
}

/// What the user reads at the moment they decide.
///
/// It used to glue three machine fields with colons and nest them in
/// parentheses — "命令请求网络:curl -sS …(原因:用户要求…)" — inside the
/// parentheses the client adds of its own. One sentence, one separator.
pub(crate) fn permission_request_description(
    action: &str,
    reason: &str,
    grants: TurnPermissionGrants,
    scope: GrantScope,
) -> String {
    let mut parts = Vec::new();
    if grants.network {
        parts.push("网络");
    }
    if grants.repository_git {
        parts.push("当前仓库 Git 元数据写入");
    }
    if grants.unrestricted_fs {
        parts.push(scope.fs_note());
    }
    let what = if parts.is_empty() {
        "权限".to_string()
    } else {
        parts.join(" + ")
    };
    let prefix = scope.prefix();
    let mut text = format!("{prefix}{what}");
    if !action.trim().is_empty() {
        text.push_str(" · ");
        text.push_str(action.trim());
    }
    if !reason.trim().is_empty() {
        text.push_str(" · 原因：");
        text.push_str(reason.trim());
    }
    text
}

pub(crate) fn permission_grant_message(granted: bool, grants: TurnPermissionGrants) -> String {
    if granted {
        permission_granted_message(grants)
    } else {
        permission_denied_by_user_message()
    }
}

fn permission_granted_message(grants: TurnPermissionGrants) -> String {
    let mut parts = Vec::new();
    if grants.network {
        parts.push("网络访问");
    }
    if grants.repository_git {
        parts.push("当前仓库 Git 元数据写入");
    }
    if grants.unrestricted_fs {
        parts.push("本轮无限制写(命令不再套工作区写沙箱)");
    }
    if parts.is_empty() {
        "已获授权。".to_string()
    } else {
        format!("已获授权:本轮允许{}。", parts.join("、"))
    }
}

/// Model-facing result when a person clicked Deny. English is protocol
/// guidance for the model, not TUI copy.
pub(crate) fn permission_denied_by_user_message() -> String {
    "The user explicitly denied this permission request. \
     Continue only with capabilities already available. \
     Do not request the same or broader permission again for this task \
     unless the user explicitly changes their decision. \
     If you now need the user to perform an action or provide information, \
     use request_user_input. \
     If the task cannot proceed without the denied capability, \
     report the task as blocked rather than repeatedly retrying."
        .to_string()
}

pub(crate) fn permission_already_denied_message() -> String {
    "The user already denied this permission for the current task. \
     Do not request it again. Continue only with capabilities already available, \
     or use request_user_input if you need the user to act."
        .to_string()
}

pub(crate) fn permission_denied_unattended_message() -> String {
    "Permission was not granted (no human was available to approve). \
     Continue only with capabilities already available. \
     Do not treat this as a user refusal."
        .to_string()
}

/// Outcome of `request_permissions`. Human Deny is not the same fact as
/// a headless auto-deny.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PermissionRequestOutcome {
    Granted {
        grants: TurnPermissionGrants,
        /// The user chose to allow it for the rest of this turn, not just
        /// what was asked. Only a single-call escalation reads it; a
        /// `request_permissions` grant already lasts the turn.
        for_turn: bool,
        message: String,
    },
    DeniedByUser {
        requested: TurnPermissionGrants,
        message: String,
    },
    DeniedUnattended {
        requested: TurnPermissionGrants,
        message: String,
    },
    Invalid {
        message: String,
    },
}

impl PermissionRequestOutcome {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Granted { message, .. }
            | Self::DeniedByUser { message, .. }
            | Self::DeniedUnattended { message, .. }
            | Self::Invalid { message } => message,
        }
    }

    pub(crate) fn is_error(&self) -> bool {
        !matches!(self, Self::Granted { .. })
    }
}

/// Apply turn grants onto a tool context (network + optional unrestricted FS).
pub(crate) fn apply_turn_grants(
    mut ctx: leveler_tools::ToolContext,
    grants: TurnPermissionGrants,
) -> leveler_tools::ToolContext {
    if grants.network {
        ctx.policy.grant_network();
    }
    if grants.repository_git {
        ctx.policy.grant_repository_git();
    }
    if grants.unrestricted_fs {
        ctx.policy.grant_unrestricted_fs();
    }
    ctx
}

/// The tool the model calls to ask for elevated permission (network / filesystem).
pub(crate) fn request_permissions_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: REQUEST_PERMISSIONS_TOOL.to_string(),
        description: "Ask for a permission grant that lasts the rest of this turn. \
            Arguments: `action` (required), optional `reason`, `network` (bool), \
            `filesystem` (`workspace`, `git`, or `unrestricted`), `full_access` \
            (bool: network plus unrestricted filesystem). A call that names none \
            of those three requests network only. `git` allows current-repository \
            Git metadata writes. `unrestricted` drops command write confinement. \
            Approval applies the named grant until the turn ends. Denial grants \
            nothing. This tool is not offered when the mode is already full access."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "description": "What you want to do that needs permission." },
                "reason": { "type": "string", "description": "Why it is necessary." },
                "network": { "type": "boolean", "description": "Request network access for this turn." },
                "filesystem": {
                    "type": "string",
                    "enum": ["workspace", "git", "unrestricted"],
                    "description": "workspace = keep write sandbox (default); git = allow current-repository Git metadata writes; unrestricted = no write confinement for commands this turn."
                },
                "full_access": {
                    "type": "boolean",
                    "description": "Shorthand for network=true and filesystem=unrestricted."
                }
            },
            "required": ["action"]
        }),
    }
}

/// Agent plan synchronization contract. The plan is the user's live view of
/// the work and the seed a resumed turn reads. The runtime records and shows it
/// but never enforces it: it does not infer statuses, does not require a
/// terminal refresh, and does not require every step to be completed.
#[cfg(test)]
mod plan_sync_contract_tests {
    const BASE_PROMPT: &str = include_str!("../prompts/base.md");

    fn plan_section() -> &'static str {
        let start = BASE_PROMPT
            .find("## Plan")
            .expect("base prompt must carry a plan section");
        let rest = &BASE_PROMPT[start + 2..];
        &BASE_PROMPT[start..start + 2 + rest.find("\n## ").unwrap_or(rest.len())]
    }

    #[test]
    fn base_prompt_says_the_plan_is_a_declaration() {
        let section = plan_section();
        for needle in [
            "update_plan",
            "declared intent",
            "not verification evidence",
        ] {
            assert!(
                section.contains(needle),
                "plan section lost `{needle}`:\n{section}"
            );
        }
    }

    /// A completed step is an observed outcome. The prompt does not script
    /// when to retry or how to rewrite the list.
    #[test]
    fn completed_means_an_observed_outcome() {
        let section = plan_section();
        assert!(
            section.contains("Only observed outcomes justify `completed`"),
            "COMPLETE lost:\n{section}"
        );
    }

    #[test]
    fn a_failed_action_is_not_reported_completed() {
        let section = plan_section();
        assert!(
            section.contains(
                "failed, denied, abandoned or unfinished work must not be reported as completed"
            ),
            "FAILURE lost:\n{section}"
        );
        assert!(
            !section.contains("stays `in_progress`"),
            "the plan contract must not tell the model to retry:\n{section}"
        );
    }

    #[test]
    fn an_abandoned_step_is_not_marked_completed() {
        let section = plan_section();
        assert!(
            section.contains("abandoned"),
            "abandoned work must not be reported completed:\n{section}"
        );
        assert!(
            !section.contains("rewrite that step"),
            "rewriting the plan is the model's decision:\n{section}"
        );
    }

    /// Timing rules for when the plan must move are strategy.
    #[test]
    fn the_plan_section_does_not_schedule_updates() {
        let section = plan_section();
        for banned in [
            "may trail",
            "already left behind",
            "Converge:",
            "- Close:",
            "before `update_goal`",
            "same response as the first tool call",
        ] {
            assert!(
                !section.contains(banned),
                "plan section must not schedule updates (`{banned}`):\n{section}"
            );
        }
    }

    /// Terminal reconciliation is freshness, never an instruction to turn all
    /// rows green. Both ordinary closeout and goal closeout state the same
    /// contract where the model reads it.
    #[test]
    fn plan_completion_gate_is_removed_but_plan_truth_remains() {
        let section = plan_section();
        assert!(
            !section.contains("- Close:") && !section.contains("before `update_goal`"),
            "the mandatory terminal Plan ceremony must be gone:\n{section}"
        );
        assert!(
            section.contains("Only observed outcomes justify `completed`")
                && section.contains("must not be reported as completed"),
            "plan truthfulness rules must remain:\n{section}"
        );
        let def = super::update_goal_tool_definition();
        assert!(
            !def.description.contains("update_plan"),
            "the goal tool must not mandate a terminal Plan refresh: {}",
            def.description
        );
        for needle in ["AS THE USER STATED IT", "false completion", "`blocked`"] {
            assert!(
                def.description.contains(needle),
                "completion semantics lost `{needle}`"
            );
        }
    }

    /// The plan is the agent's declared progress. The runtime records, persists
    /// and shows it; it does not observe whether the work is really at the
    /// declared step. The declaration framing lives in the prompt section the
    /// model reads; the tool contract states that the runtime advances nothing
    /// on its own. Neither text may present the plan as the live state of the
    /// work.
    #[test]
    fn the_plan_is_described_as_declared_progress() {
        let def = crate::update_plan::UpdatePlanTool;
        let description = leveler_tools::Tool::description(&def);
        let section = plan_section();
        assert!(
            section.contains("declared intent"),
            "plan section must frame the plan as a declaration:\n{section}"
        );
        assert!(
            description.contains("nothing else advances"),
            "the tool contract says the runtime advances nothing:\n{description}"
        );
        for text in [section, description] {
            assert!(
                !text.contains("state of the work"),
                "plan must not be framed as the live state of the work:\n{text}"
            );
        }
    }

    /// Order is the model's decision: the contract records the list it is sent
    /// and makes no later step conditional on an earlier one.
    #[test]
    fn the_plan_contract_imposes_no_order_rule() {
        let def = crate::update_plan::UpdatePlanTool;
        let description = leveler_tools::Tool::description(&def);
        for banned in [
            "intended order",
            "completed in order",
            "sequentially",
            "one at a time",
        ] {
            assert!(
                !description.contains(banned),
                "update_plan imposes an order rule (`{banned}`):\n{description}"
            );
        }
    }

    #[test]
    fn plan_sync_is_described_without_a_completion_mandate() {
        let section = plan_section();
        assert!(
            !section.contains("freshness requirement")
                && !section.contains("Close:")
                && !section.contains("before `update_goal`"),
            "the terminal Plan mandate must be gone:\n{section}"
        );
        assert!(
            section.contains("declared intent and status"),
            "the plan stays a declaration:\n{section}"
        );
    }
}

#[cfg(test)]
mod scope_fidelity_tests {
    /// ICG-6R: the completion contract must keep saying, in so many words,
    /// that an unsatisfiable objective is `blocked` — never reinterpreted into
    /// a weaker task and completed. A wording regression here re-opens the
    /// scope-substitution false-completion path.
    #[test]
    fn update_goal_contract_forbids_scope_substitution() {
        let def = super::update_goal_tool_definition();
        for needle in [
            "AS THE USER STATED IT",
            "Partial conflicts",
            "do NOT",
            "false completion",
            "revert edits",
        ] {
            assert!(
                def.description.contains(needle),
                "completion contract lost `{needle}`"
            );
        }
    }

    /// The completion gate is mechanical and command-shaped, not a named
    /// proof tool: the model chooses what verifies its objective, and the
    /// runtime only checks that the cited command ran and passed after the
    /// last change. No specific tool name is mandated, and no wording may
    /// pretend the harness itself knows the work is good.
    #[test]
    fn update_goal_does_not_mandate_a_named_proof_tool() {
        let def = super::update_goal_tool_definition();
        for banned in [
            "PROVEN",
            "since your last edit",
            "build/tests",
            "build and tests",
        ] {
            assert!(
                !def.description.contains(banned),
                "verification must stay evidence, not a named proof tool: `{banned}`"
            );
        }
        for needle in [
            "AS THE USER STATED IT",
            "false completion",
            "`blocked`",
            "against the current workspace state",
            "verification",
            "execution record",
        ] {
            assert!(
                def.description.contains(needle),
                "completion semantics lost `{needle}`"
            );
        }
    }

    /// With production semantics finalized, there is no Plan close-coaching
    /// seam left to ablate: the tool description never mandates a terminal
    /// Plan refresh and always keeps the completion contract.
    #[test]
    fn update_goal_never_mandates_a_terminal_plan_refresh() {
        let def = super::update_goal_tool_definition();
        for banned in [
            "first call update_plan",
            "You may call update_plan then update_goal",
            "unfinished steps stay pending",
            "PROVEN",
        ] {
            assert!(
                !def.description.contains(banned),
                "production must not carry `{banned}`: {}",
                def.description
            );
        }
        for needle in ["AS THE USER STATED IT", "false completion", "`blocked`"] {
            assert!(
                def.description.contains(needle),
                "completion contract lost `{needle}`"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A requirement kind is tied to `target` in prose only when one clause
    /// names the kind and the field together.
    fn schema_states_target_for(description: &str, kind: &str) -> bool {
        description
            .split([';', '.', '\n'])
            .any(|clause| clause.contains(&format!("`{kind}`")) && clause.contains("`target`"))
    }

    /// The model reads the schema; the runtime enforces it. Both requirement
    /// declarations must state the same conditional `target` contract, derived
    /// from the runtime's own `requires_target` truth. The first Goal
    /// submission in real sessions omitted `target` on `artifact` because the
    /// schema said only `content`/`command`/`tests` needed one.
    #[test]
    fn requirement_schemas_state_the_runtime_target_contract() {
        use leveler_lifecycle::RequirementKind;

        const ALL_KINDS: [&str; 10] = [
            "artifact",
            "artifact_absent",
            "command",
            "named",
            "content",
            "tests",
            "forbidden",
            "baseline",
            "preparation",
            "constraint",
        ];
        for (tool_name, items) in [
            (
                COMMIT_TASK_TOOL,
                commit_task_tool_definition().input_schema["properties"]["requirements"]["items"]
                    .clone(),
            ),
            (
                DECLARE_SCOPE_TOOL,
                declare_scope_tool_definition().input_schema["properties"]["operations"]["items"]
                    .clone(),
            ),
        ] {
            let declared: Vec<&str> = items["properties"]["kind"]["enum"]
                .as_array()
                .expect("kind enum")
                .iter()
                .map(|value| value.as_str().expect("kind name"))
                .collect();
            assert_eq!(
                declared, ALL_KINDS,
                "{tool_name} must expose every requirement kind"
            );
            let kind_description = items["properties"]["kind"]["description"]
                .as_str()
                .expect("kind description");
            let target_description = items["properties"]["target"]["description"]
                .as_str()
                .expect("target description");
            for name in ALL_KINDS {
                let requires_target = RequirementKind::parse(name)
                    .expect("a schema kind parses")
                    .requires_target();
                let stated = schema_states_target_for(kind_description, name)
                    || schema_states_target_for(target_description, name);
                assert_eq!(
                    stated, requires_target,
                    "{tool_name}: the schema must state the target contract for `{name}` \
                     (requires_target={requires_target}). kind: {kind_description}\n\
                     target: {target_description}"
                );
            }
        }
    }

    /// The Chat delegation surface is a fixed cost on every ordinary turn.
    /// Keep it small and make the number visible to the performance record.
    ///
    /// The budget includes the structured `requirements` declaration: the
    /// alternative is a second `declare_scope` round after the commit, which
    /// costs one full model request (thousands of cached tokens and a round
    /// trip) instead of ~300 definition tokens.
    #[test]
    fn commit_task_surface_stays_small() {
        let tool = commit_task_tool_definition();
        let tokens = leveler_model::estimate_text(&tool.description)
            + leveler_model::estimate_text(&tool.input_schema.to_string());
        let guidance = leveler_model::estimate_text(TASK_COMMIT_GUIDANCE);
        println!("commit_task schema tokens: {tokens}; guidance tokens: {guidance}");
        assert!(tokens < 1000, "commit_task surface grew to {tokens} tokens");
        assert!(guidance < 300, "task guidance grew to {guidance} tokens");
    }

    /// The acceptance list stays a bounded payload; scale is handled by
    /// grouping related deliverables under one check, never by letting the
    /// declaration grow with a possibly inflated extraction set.
    #[test]
    fn commit_task_acceptance_capacity_stays_bounded_and_grouping_is_required() {
        let tool = commit_task_tool_definition();
        let acceptance = &tool.input_schema["properties"]["acceptance"];
        assert_eq!(acceptance["maxItems"], 20);
        assert!(
            acceptance["description"].as_str().is_some_and(
                |text| text.contains("one check may name several related deliverables")
            ),
            "the schema must state the grouping contract: {acceptance}"
        );
    }

    /// The completion account has the same dynamic cap as the commit
    /// declaration. A fixed `maxItems` in the schema would reject a legitimate
    /// long task before `parse_accounting` could apply the real bound.
    #[test]
    fn update_goal_accounting_has_no_fixed_schema_cap() {
        let tool = update_goal_tool_definition();
        let requirements = &tool.input_schema["properties"]["requirements"];
        assert!(
            requirements.get("maxItems").is_none(),
            "the accounting cap is the extracted requirement set, not a schema constant"
        );
    }

    /// The upgrade decision is semantic evidence about the request, not a
    /// mechanical trigger. A question, a status/verification check and an
    /// explanation must all be excluded by name, and no counting rule (message
    /// length, tool count, keyword list) may appear as the trigger.
    #[test]
    fn the_task_upgrade_needs_evidence_and_never_counts_words() {
        let def = commit_task_tool_definition();
        let rule = format!("{} {}", def.description, TASK_COMMIT_GUIDANCE);
        for required in [
            "perform work",
            "concrete result",
            "several steps",
            "question",
            "status or verification check",
            "explanation",
            "if unsure, do not",
        ] {
            assert!(
                rule.contains(required),
                "the upgrade rule must state {required:?}: {rule}"
            );
        }
        for forbidden in [
            "two or more",
            "at least three",
            "characters",
            "tokens",
            "tool calls",
            "number of",
        ] {
            assert!(
                !rule.to_lowercase().contains(forbidden),
                "the upgrade rule must not be a mechanical trigger ({forbidden:?}): {rule}"
            );
        }
    }

    #[test]
    fn worker_scope_advertises_directory_grants() {
        // Ergonomics (MA-WA1): at decision time Main often knows the module,
        // not every file. Directory scope has always been enforced (allowlist
        // + overlap both use directory-prefix semantics) — the schema must say
        // so, or the model believes it needs perfect file knowledge to spawn.
        let def = spawn_agent_tool_definition();
        let files_desc = def.input_schema["properties"]["files"]["description"]
            .as_str()
            .unwrap();
        assert!(
            files_desc.contains("Legacy") && files_desc.contains("claim"),
            "files must read as the legacy path, pointing at claim_write_scope: {files_desc}"
        );
        // Late-bound ownership: the claim tool advertises directory grants.
        let claim = claim_write_scope_tool_definition();
        let claim_desc = claim.input_schema["properties"]["paths"]["description"]
            .as_str()
            .unwrap();
        assert!(
            claim_desc.contains("director"),
            "claim paths must advertise directory grants: {claim_desc}"
        );
    }

    /// V2 four-point background contract, tool-description + parameter points:
    /// the advertised default is background, the call returns immediately with
    /// the child's id, settlement is promised, and the foreground escape is
    /// scoped to "next action depends on it". Runtime point is
    /// `resolve_run_in_background`; prompt point is the steer hint (tested in
    /// sub_agent.rs).
    #[test]
    fn spawn_agent_advertises_the_background_first_contract() {
        let def = spawn_agent_tool_definition();
        let d = def.description.to_ascii_lowercase();
        assert!(d.contains("defaults to true"), "{}", def.description);
        assert!(
            d.contains("returns the child's id immediately") || d.contains("returns immediately"),
            "{}",
            def.description
        );
        assert!(d.contains("settle"), "{}", def.description);
        assert!(
            d.contains("claim_write_scope") || (d.contains("claim") && d.contains("write")),
            "the late-bound claim protocol must be model-visible: {}",
            def.description
        );
        assert!(
            !d.contains("keep the work yourself"),
            "delegation choice is not a tool instruction: {}",
            def.description
        );
        assert!(
            !d.contains("when the user asks for parallel/multi-agent work"),
            "{}",
            def.description
        );
        assert!(!d.contains("always spawn"), "{}", def.description);

        let param = def.input_schema["properties"]["run_in_background"]["description"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(param.contains("defaults to true"), "{param}");
        assert!(param.contains("waits for the child's result"), "{param}");
        assert!(!param.contains("next action depends"), "{param}");
    }

    /// The child's short task title is spawn-time identity, separate from the
    /// full `task` instructions. The schema has to require it and say how it
    /// differs, or the UI has only the instructions to show.
    #[test]
    fn spawn_agent_requires_a_separate_short_task_title() {
        let def = spawn_agent_tool_definition();
        assert_eq!(
            def.input_schema["required"],
            serde_json::json!(["task", "title"]),
            "title is part of the spawn contract"
        );
        let title = def.input_schema["properties"]["title"]["description"]
            .as_str()
            .unwrap();
        assert!(
            title.to_ascii_lowercase().contains("not a copy of `task`"),
            "{title}"
        );
        assert!(title.contains("3–8 words"), "{title}");
        assert!(
            def.description.contains("short display name"),
            "{}",
            def.description
        );
    }

    #[test]
    fn run_in_background_default_is_runtime_resolved_not_model_remembered() {
        assert!(resolve_run_in_background(&serde_json::json!({"task": "x"})));
        assert!(resolve_run_in_background(
            &serde_json::json!({"task": "x", "run_in_background": true})
        ));
        assert!(!resolve_run_in_background(
            &serde_json::json!({"task": "x", "run_in_background": false})
        ));
        // Malformed value degrades to the advertised default, never to a
        // silent foreground surprise.
        assert!(resolve_run_in_background(
            &serde_json::json!({"task": "x", "run_in_background": "yes"})
        ));
    }

    #[test]
    fn legacy_action_only_requests_network() {
        let (_, _, g) = parse_permission_request(&serde_json::json!({"action": "curl"}));
        assert_eq!(
            g,
            TurnPermissionGrants {
                network: true,
                repository_git: false,
                unrestricted_fs: false
            }
        );
    }

    #[test]
    fn full_access_shorthand_sets_both() {
        let (_, _, g) =
            parse_permission_request(&serde_json::json!({"action": "x", "full_access": true}));
        assert!(g.network && g.unrestricted_fs);
    }

    #[test]
    fn filesystem_unrestricted_without_network() {
        let (_, _, g) = parse_permission_request(&serde_json::json!({
            "action": "write outside",
            "network": false,
            "filesystem": "unrestricted"
        }));
        assert!(!g.network && g.unrestricted_fs);
    }

    #[test]
    fn repository_git_permission_is_narrower_than_unrestricted_filesystem() {
        let (_, _, g) = parse_permission_request(&serde_json::json!({
            "action": "git commit",
            "filesystem": "git"
        }));
        assert!(!g.network);
        assert!(g.repository_git);
        assert!(!g.unrestricted_fs);

        let unrestricted = TurnPermissionGrants {
            network: false,
            repository_git: false,
            unrestricted_fs: true,
        };
        assert!(unrestricted.covers(g));
    }

    #[test]
    fn repository_git_grant_keeps_workspace_confinement() {
        let dir = tempfile::tempdir().unwrap();
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx =
            leveler_tools::ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let elevated = apply_turn_grants(
            ctx,
            TurnPermissionGrants {
                network: false,
                repository_git: true,
                unrestricted_fs: false,
            },
        );
        assert_eq!(
            elevated.write_scope(),
            leveler_execution::WriteScope::WorkspaceWithGit {
                root: dir.path().canonicalize().unwrap()
            }
        );
    }

    #[test]
    fn apply_turn_grants_sets_network_and_fs_flags() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-grant-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx =
            leveler_tools::ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted)
                .with_sandbox(true);
        assert!(ctx.policy.network_denied());
        assert!(!ctx.policy.unrestricted_fs());
        let elevated = apply_turn_grants(
            ctx,
            TurnPermissionGrants {
                network: true,
                repository_git: false,
                unrestricted_fs: true,
            },
        );
        assert!(!elevated.policy.network_denied());
        assert!(elevated.policy.unrestricted_fs());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn human_denial_message_is_not_a_bare_false() {
        let msg = permission_denied_by_user_message();
        assert!(msg.contains("explicitly denied"));
        assert!(msg.contains("request_user_input"));
        assert!(!msg.contains("用户未批准"));
        let unattended = permission_denied_unattended_message();
        assert!(!unattended.contains("explicitly denied"));
        assert!(unattended.contains("no human"));
    }

    #[test]
    fn request_permissions_schema_advertises_filesystem_fields() {
        let tool = request_permissions_tool_definition();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("network"));
        assert!(props.contains_key("filesystem"));
        assert!(props.contains_key("full_access"));
    }

    #[test]
    fn update_goal_exposes_optional_structured_next_step() {
        let tool = update_goal_tool_definition();
        let properties = tool.input_schema["properties"].as_object().unwrap();
        assert!(properties.contains_key("next_step"));
        assert!(
            !properties.contains_key("override_incomplete_todos"),
            "the plan is not a completion gate, so there is nothing to override"
        );
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(
            !required.iter().any(|field| field == "next_step"),
            "next_step must be omitted when there is no genuine follow-up"
        );
    }

    #[test]
    fn request_user_input_advertises_multi_question_shape() {
        let tool = request_user_input_tool_definition();
        let questions = &tool.input_schema["properties"]["questions"];
        assert_eq!(questions["type"], "array");
        let item = &questions["items"];
        for field in [
            "header",
            "question",
            "kind",
            "options",
            "allow_other",
            "min_choices",
            "max_choices",
        ] {
            assert!(
                item["properties"].get(field).is_some(),
                "the question schema must carry `{field}`: {item}"
            );
        }
        let kinds = item["properties"]["kind"]["enum"].as_array().unwrap();
        assert_eq!(
            kinds.iter().filter_map(|k| k.as_str()).collect::<Vec<_>>(),
            vec!["single", "multi", "text"]
        );
        // Neither form may be forced on the model: a single-question call
        // omits `questions`, and a multi-question call may omit the headline.
        assert!(
            !tool.input_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f == "questions" || f == "question"),
            "both shapes must stay optional: {}",
            tool.input_schema
        );
    }

    #[test]
    fn request_user_input_is_primary_with_ask_user_alias() {
        assert!(is_user_input_tool(REQUEST_USER_INPUT_TOOL));
        assert!(is_user_input_tool(ASK_USER_TOOL));
        assert!(!is_user_input_tool("request_permissions"));

        let primary = request_user_input_tool_definition();
        assert_eq!(primary.name, "request_user_input");
        assert!(
            !primary.description.contains("ask_user"),
            "the legacy name is accepted by the parser and is not advertised"
        );
        assert!(primary.input_schema["properties"].get("question").is_some());
        assert!(
            primary.description.contains("mutually exclusive")
                || primary.description.contains("2–4")
                || primary.description.contains("2-4"),
            "tool description must state the choice shape: {}",
            primary.description
        );
        assert!(
            primary.description.contains("does not pause"),
            "prose must not be described as a wait: {}",
            primary.description
        );
    }

    /// The prompt is the user's only view of what they are approving, and it
    /// is read at the moment a security decision is made. It used to be three
    /// machine fields glued with colons and nested parentheses:
    /// "命令请求网络:curl -sS …(原因:用户要求确认…)" — and the client wraps it
    /// in parentheses of its own.
    #[test]
    fn an_escalation_prompt_reads_as_a_sentence() {
        let grants = TurnPermissionGrants {
            network: true,
            repository_git: false,
            unrestricted_fs: false,
        };
        let text = permission_request_description(
            "curl -sS https://registry.npmjs.org/-/ping",
            "确认能否连到 npm registry",
            grants,
            GrantScope::SingleCall,
        );
        assert!(!text.contains('('), "no machine parentheses: {text}");
        assert!(
            !text.contains("网络:") && !text.contains("原因:"),
            "no colon-glued fields: {text}"
        );
        assert!(text.contains("网络"), "{text}");
        assert!(text.contains("curl -sS"), "{text}");
        assert!(text.contains("确认能否连到 npm registry"), "{text}");
    }

    /// The prompt is the user's only view of what they are approving. A
    /// one-call escalation must not be described as lasting the whole turn.
    #[test]
    fn a_single_call_escalation_prompt_does_not_claim_the_turn() {
        let grants = TurnPermissionGrants {
            network: false,
            repository_git: false,
            unrestricted_fs: true,
        };
        let turn = permission_request_description("git pull", "", grants, GrantScope::Turn);
        assert!(turn.contains("本轮"), "{turn}");

        // A command's escalation lasts as long as the option the user picks
        // ("仅允许本次" / "本轮对话内允许"); the description claims neither.
        let once = permission_request_description("git pull", "", grants, GrantScope::SingleCall);
        assert!(!once.contains("本轮"), "{once}");
        assert!(!once.contains("仅此一次"), "{once}");
        assert!(once.contains("写不受工作区沙箱限制"), "{once}");
    }

    #[test]
    fn escalation_is_absent_when_the_call_carries_no_escalate_field() {
        assert!(parse_escalation(&serde_json::json!({"cmd": "git pull"})).is_none());
    }

    #[test]
    fn escalation_parses_the_same_axes_as_request_permissions() {
        let (reason, grants) = parse_escalation(&serde_json::json!({
            "cmd": "git pull --rebase",
            "escalate": { "reason": "remote git needs the network and .git writes", "full_access": true }
        }))
        .expect("escalate must parse");
        assert!(reason.contains("remote git"));
        assert!(grants.network && grants.unrestricted_fs);

        let (_, fs_only) = parse_escalation(&serde_json::json!({
            "escalate": { "reason": "write outside the workspace", "filesystem": "unrestricted" }
        }))
        .unwrap();
        assert!(fs_only.unrestricted_fs && !fs_only.network);
    }

    /// Unlike `request_permissions`, a bare `escalate` must NOT silently mean
    /// network: the model has to name the axis its command was denied on.
    #[test]
    fn an_escalation_naming_no_axis_is_empty_not_a_network_grant() {
        let (_, grants) = parse_escalation(&serde_json::json!({
            "escalate": { "reason": "please" }
        }))
        .unwrap();
        assert!(grants.is_empty(), "a bare escalate must not grant network");
    }

    #[test]
    fn escalation_is_advertised_on_command_tools_only() {
        let mut tools = vec![
            ToolDefinition {
                name: "shell_command".to_string(),
                description: String::new(),
                input_schema: serde_json::json!({"type":"object","properties":{"cmd":{"type":"string"}}}),
            },
            ToolDefinition {
                name: "run_command".to_string(),
                description: String::new(),
                input_schema: serde_json::json!({"type":"object","properties":{"program":{"type":"string"}}}),
            },
            ToolDefinition {
                name: "read_file".to_string(),
                description: String::new(),
                input_schema: serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
            },
        ];
        advertise_escalation(&mut tools);

        for tool in tools.iter().filter(|t| t.name != "read_file") {
            let escalate = tool.input_schema["properties"]
                .get("escalate")
                .unwrap_or_else(|| panic!("{} must advertise escalate", tool.name));
            let props = escalate["properties"].as_object().unwrap();
            for axis in ["reason", "network", "filesystem", "full_access"] {
                assert!(props.contains_key(axis), "{} missing {axis}", tool.name);
            }
            assert_eq!(escalate["required"], serde_json::json!(["reason"]));
        }
        assert!(
            tools[2].input_schema["properties"]
                .get("escalate")
                .is_none(),
            "a read-only tool has nothing to escalate"
        );
        assert!(
            tools[0].input_schema["properties"].get("cmd").is_some(),
            "augmenting the schema must not drop the tool's own fields"
        );
    }
}

#[cfg(test)]
mod receipt_tests {
    /// The no-warning receipt is unchanged: an ordinary completion keeps one
    /// short line, and every existing UI contract that reads it stays valid.
    #[test]
    fn an_unqualified_completion_keeps_its_one_line_receipt() {
        assert_eq!(
            super::completion_receipt("Goal completed", &[]),
            "Goal completed."
        );
        assert_eq!(
            super::completion_receipt("Goal blocked", &[]),
            "Goal blocked."
        );
    }

    /// A boundary the runtime computed reaches the model that writes the
    /// answer, in the same words the user sees.
    #[test]
    fn a_boundary_is_stated_on_the_acceptance() {
        let warnings = vec![
            "requirement `全部功能` is reported done but has no mechanical check; the completion \
             rests on the model's account, not runtime evidence"
                .to_string(),
        ];
        let receipt = super::completion_receipt("Goal completed", &warnings);
        assert!(receipt.starts_with("Goal completed."), "{receipt}");
        assert!(receipt.contains("全部功能"), "{receipt}");
        assert!(receipt.contains("not on evidence"), "{receipt}");
        assert!(
            receipt.contains("must not present these as verified"),
            "{receipt}"
        );
        // Bounded: a long completion cannot turn its receipt into a wall.
        let many: Vec<String> = (0..9).map(|i| format!("boundary {i}")).collect();
        let receipt = super::completion_receipt("Goal completed", &many);
        assert!(receipt.contains("…and 3 more"), "{receipt}");
        assert!(!receipt.contains("boundary 6"), "{receipt}");
    }
}
