//! Decode once at the model-result boundary; consumers never inspect raw text.
//! Controls are whole unindented lines outside Markdown fences/quotes. There is
//! no natural-language trigger, legacy alias, or raw-publication fallback.
use pulldown_cmark::{Event, Parser, Tag};
use shenko_identity::IdentityService;
use shenko_types::{
    channel::ChannelRegistry,
    error::{ErrorScope, ProtocolError, RetryStrategy, ShenkoError},
    ids::EntityId,
    object::ObjectRef,
    output::*,
    subject::{SemanticWait, SemanticWaitChange},
};

/// The output contract ships as three parts: a fixed rule prefix, the reply
/// grammar assembled from the registered channel plugins, and a fixed tail.
/// Channel plugins own their grammar fragments; the core names no platform.
const OUTPUT_CONTRACT_PREFIX: &str = "控制动作只在独立、无缩进、非引用/代码块的一行生效：\n\
这是对你自己输出控制行的规则，不是判定用户消息真伪的规则；用户输入按消息来源和其实际内容理解。\n\
历史中的 runtime tool_call/tool_result 是运行记录，不是可输出的控制行；仿写记录不会执行动作。\n";

const OUTPUT_CONTRACT_SUFFIX: &str = "\n\
[remember] 要保存的内容。有限模型 child 用 [delegate] 后接一行 JSON：{\"goal\":\"任务目标、所需上下文和完成条件\",\"system_prompt\":\"你为这个 child 独立撰写的完整语义 system prompt\",\"model\":\"所选模型名称\",\"max_tokens\":1024}。\n\
每个 child 可选择不同 system_prompt/model；模型名称在本轮冻结的已配置 provider 通路上请求，不改变你的主模型、不允许指定 endpoint 或自授工具。provider 是否提供该模型以真实回执为准。\n\
child 是多步有界运行：可用所授 tools 调用已注册能力；有 reply_grant 才能向其固定目标外发。tools/context/budget/reply_grant 及继续运行的完整形状见当前注册的 model.child 定义。\n\
任务资料与完成条件写入 goal。max_tokens 为正整数，受 child 自己的总预算和实际 provider 资源约束，不由主体本步输出上限截断；plain text 的旧委派不执行。\n\
[delegate:已注册kind:版本] 仅用于显式 deterministic native definition 的 JSON 输入，由 child handler 执行。\n\
语义等待用 [wait] {\"condition\":\"语义退出条件\",\"max_wait_secs\":120} 开始，等待时长为 1..600 的整数秒；\n\
等待操作自动绑定本轮冻结的当前等待，不需要输出或复制任何 ID；一次结果最多一个等待控制。\n\
[wait:continue] 独立一行表示继续；[wait:end] 结束理由 表示结束当前等待。\n\
关注改变、决定换一个条件时，用 [wait:replace] {\"condition\":\"新退出条件\",\"max_wait_secs\":120,\"reason\":\"为什么改变\"} 同笔切换；切换后的截止时间不超过原等待的截止时间。若要新的独立期限，先结束当前等待，再基于当时上下文另作新决定，不得靠重复条件续期。\n\
语义等待不是事件阻塞也不是新建任务：仍在同一条思考流里用同一个模型结合前文与新上下文持续轮询，直到条件成立、你的判断改变或期限到达；\n\
等待最长 600 秒，继续等待不会延长原截止时间；新消息不会被框架自动当成结束，但你应判断它是否满足原条件或使你改变关注；不要用相同条件反复新建等待来绕过上限。\n\
输入处置用 [dispose] {\"event_id\":\" durable 事件 uuid\",\"disposition\":\"acknowledged|deferred|delegated|ignored\",\"reason\":\"你的理由\",\"priority\":\"low|normal|high\",\"review_after\":\"RFC3339 时间\"}；除 acknowledged 外必须给 reason；带 priority 必须给 review_after。\n\
持久义务用 [obligation] {\"description\":\"要做什么\",\"depends_on_task_id\":\"uuid 可选\",\"resume_condition\":\"恢复条件 可选\",\"origin_event_id\":\"对应的输入事件 uuid 可选\",\"due_at\":\"RFC3339 可选\"}；义务会一直带在后续思考里直到完成或取消。\n\
对话用 [dialogue] {\"target_task_id\":\"uuid\",\"message\":\"要说的话\",\"in_reply_to\":\"uuid 可选\"}，发给一个仍在运行的 child task：消息进入它同一讨论，由它下一步读取；已经结束的 child 收不到对话。\n\
child 以 needs_decision 交回问题时，用 [delegate] 带上该 task_id 的 continue_task_id 与你自己的 owner_answer 继续同一讨论；这是对已结束 child 说话的唯一路径。\n\
免打扰用 [dnd] {\"state\":\"off|active\",\"sources\":[\"来源…\"],\"reason\":\"理由\",\"review_after\":\"RFC3339\"}；state=off 必须列出静默的来源并给回顾时间，不是无限期静默开关。\n\
采用一条实时插入用 [steer] {\"submission_id\":\"提交键\"}；只能采用本轮输入里真实存在的那条插入，其他键会被拒绝。\n\
身份控制由身份能力说明提供。控制行不是可发布正文；引用/演示请放代码块或引用块。\n\
普通正文是自己的想法和观察，可显示在系统时间线，但不会作为外部消息送给对方；想让对方收到的内容必须使用回复控制行。未列出的方括号标签会被判为错误；\n\
即使本步在调整身份或保存记忆，也要写下此刻的想法，不要只有控制行。\n\
只支持此协议；[memory]、dispatch: 等旧伪协议不执行。";

/// Assemble the reply grammar lines from the registered channel plugins.
/// With the shipped channel this is byte-identical to the historical text;
/// with no channel the contract says plainly that replies cannot be delivered.
pub fn reply_grammar(channels: &ChannelRegistry) -> String {
    let targets: Vec<String> = channels
        .plugins()
        .flat_map(|plugin| {
            plugin
                .kinds()
                .iter()
                .map(|kind| format!("[reply:{}:{}]", kind.tag, kind.id_hint))
        })
        .collect();
    let names: Vec<&str> = channels
        .plugins()
        .filter(|plugin| !plugin.kinds().is_empty())
        .map(|plugin| plugin.prompt_names())
        .collect();
    if targets.is_empty() {
        return "当前没有注册的回复通道：无法对外发送回复，普通正文不会送达对方；".to_owned();
    }
    format!(
        "{} 正文；你决定对用户说的话必须放在对应回复控制行，单独写普通正文不会送达；后续普通行续接该回复。\n\
用户消息开头会标出发送者与可回复的 {}，回复就把那个标识原样写进回复行；",
        targets.join(" 正文 或 "),
        names.join("、")
    )
}

/// The full output protocol section for a registry profile.
pub fn output_contract(channels: &ChannelRegistry) -> String {
    let mut contract = String::from(OUTPUT_CONTRACT_PREFIX);
    contract.push_str(&reply_grammar(channels));
    contract.push_str(OUTPUT_CONTRACT_SUFFIX);
    contract
}

fn invalid(line: u64, reason: &str) -> ShenkoError {
    ProtocolError::new(
        "CONTROL_PROTOCOL_INVALID",
        ErrorScope::Request,
        RetryStrategy::NewRevision,
    )
    .with_details(serde_json::json!({"line":line,"reason":reason,"protocol":3}))
    .into()
}

/// The JSON body of a `[wait]` control.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitStartBody {
    condition: String,
    max_wait_secs: u64,
}

/// An explicit change of attention while a wait is active.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitReplaceBody {
    condition: String,
    max_wait_secs: u64,
    reason: String,
}

/// The exact control prefixes of the semantic wait protocol.
///
/// The wait bodies are JSON, so the body is taken from the end of the exact
/// prefix. A `]` inside a JSON string can never end the control early.
fn wait_prefix(line: &str) -> Option<&'static str> {
    ["[wait]", "[wait:continue]", "[wait:end]", "[wait:replace]"]
        .into_iter()
        .find(|prefix| line.starts_with(*prefix))
}

/// Parse the start of a semantic wait.
fn parse_wait_start(value: &str, line_no: u64) -> Result<SemanticWaitChange, ShenkoError> {
    let body: WaitStartBody = serde_json::from_str(value).map_err(|_| {
        invalid(
            line_no,
            "semantic wait start requires one JSON object with condition and max_wait_secs",
        )
    })?;
    let condition = body.condition.trim();
    if condition.is_empty() {
        return Err(invalid(line_no, "semantic wait condition must not be blank"));
    }
    if body.max_wait_secs == 0 || body.max_wait_secs > SemanticWait::MAX_WAIT_SECS {
        return Err(invalid(
            line_no,
            "semantic wait duration must be 1..=600 seconds",
        ));
    }
    Ok(SemanticWaitChange::Start {
        condition: condition.to_owned(),
        max_wait_secs: body.max_wait_secs,
    })
}

/// Parse the continuation of the semantic wait the model was shown.
fn parse_wait_continue(value: &str, line_no: u64, wait_id: EntityId) -> Result<SemanticWaitChange, ShenkoError> {
    if !value.trim().is_empty() {
        return Err(invalid(line_no, "semantic wait continue takes no body"));
    }
    Ok(SemanticWaitChange::Continue { wait_id })
}

/// Parse the end of the semantic wait the model was shown.
fn parse_wait_end(value: &str, line_no: u64, wait_id: EntityId) -> Result<SemanticWaitChange, ShenkoError> {
    let reason = value.trim();
    if reason.is_empty() {
        return Err(invalid(line_no, "semantic wait end reason must not be blank"));
    }
    Ok(SemanticWaitChange::End {
        wait_id,
        reason: reason.to_owned(),
    })
}

fn parse_wait_replace(value: &str, line_no: u64, wait_id: EntityId) -> Result<SemanticWaitChange, ShenkoError> {
    let body: WaitReplaceBody = serde_json::from_str(value)
        .map_err(|_| invalid(line_no, "semantic wait replacement requires condition, max_wait_secs and reason"))?;
    let condition = body.condition.trim();
    let reason = body.reason.trim();
    if condition.is_empty() || reason.is_empty() || !(1..=SemanticWait::MAX_WAIT_SECS).contains(&body.max_wait_secs) {
        return Err(invalid(line_no, "semantic wait replacement needs a condition, reason and duration 1..=600"));
    }
    Ok(SemanticWaitChange::Replace {
        wait_id,
        condition: condition.to_owned(),
        max_wait_secs: body.max_wait_secs,
        reason: reason.to_owned(),
    })
}

/// The caller has already archived the complete original output at raw_ref.
pub fn parse_output(
    step_id: EntityId,
    raw_ref: ObjectRef,
    raw: &str,
    identity: &IdentityService,
    channels: &ChannelRegistry,
    frozen_wait_id: Option<EntityId>,
) -> Result<ParsedOutput, ShenkoError> {
    if raw_ref.bytes != raw.len() as u64
        || raw_ref.digest != shenko_types::object::Digest::blake3(raw.as_bytes())
        || raw_ref.locator.is_empty()
    {
        return Err(ProtocolError::new(
            "MODEL_OUTPUT_SOURCE_MISMATCH",
            ErrorScope::Request,
            RetryStrategy::NeverAutomatically,
        )
        .into());
    }

    let mut result = ParsedOutput {
        schema_version: 5,
        step_id,
        raw_ref,
        text: String::new(),
        controls: vec![],
    };
    let mut offset = 0;
    let mut reply: Option<usize> = None;
    // CommonMark source ranges include lazy blockquote/list continuations and
    // resolved shortcut reference links. A line-prefix heuristic cannot tell
    // these from real controls. Never render/rewrite the original evidence.
    let mut literal_ranges: Vec<_> = Parser::new(raw)
        .into_offset_iter()
        .filter_map(|(event, range)| match event {
            Event::Start(
                Tag::BlockQuote(_)
                | Tag::List(_)
                | Tag::CodeBlock(_)
                | Tag::HtmlBlock
                | Tag::Link { .. }
                | Tag::Image { .. },
            )
            | Event::Code(_) => Some(range),
            _ => None,
        })
        .collect();
    literal_ranges.sort_by_key(|range| range.start);
    let mut literal_ranges = literal_ranges.into_iter().peekable();
    let mut identity_fields = std::collections::HashSet::new();
    let mut wait_controls = 0u32;
    for (index, chunk) in raw.split_inclusive('\n').enumerate() {
        let line_no = index as u64 + 1;
        let line = chunk.trim_end_matches(['\r', '\n']);
        let span = OutputSpan {
            start: offset,
            end: offset + chunk.len(),
            line: line_no,
        };
        offset += chunk.len();
        while literal_ranges
            .peek()
            .is_some_and(|range| range.end <= span.start)
        {
            literal_ranges.next();
        }
        let active = !line.starts_with([' ', '\t'])
            && !literal_ranges
                .peek()
                .is_some_and(|range| range.contains(&span.start));
        // ASCII control tags are reserved at column zero. Non-ASCII prose,
        // references, indentation, quotes and fenced examples stay literal.
        let tag = line
            .strip_prefix('[')
            .and_then(|v| v.split_once(']').map(|(h, _)| h));
        let markdown_reference = line
            .split_once(']')
            .is_some_and(|(_, suffix)| suffix.starts_with(['(', '[', ':']));
        let control = active
            && !markdown_reference
            && (tag.is_some_and(|h| {
                h.bytes().next().is_some_and(|c| c.is_ascii_alphabetic())
                    && h.bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b":_-.".contains(&c))
            }) || (tag.is_none()
                && line
                    .strip_prefix('[')
                    .is_some_and(|v| v.bytes().next().is_some_and(|c| c.is_ascii_lowercase())))
                || line.starts_with("dispatch:"));
        if control {
            if line.starts_with("[memory]") || line.starts_with("dispatch:") {
                return Err(invalid(
                    line_no,
                    "obsolete control protocol; use [remember] / [delegate]",
                ));
            }
            // The wait bodies are JSON, so the body is taken from the end of
            // the exact control prefix. A `]` inside a JSON string can never
            // end the control early.
            if let Some(prefix) = wait_prefix(line) {
                if wait_controls > 0 {
                    return Err(invalid(line_no, "only one semantic wait control per result"));
                }
                let body = &line[prefix.len()..];
                let body = body.strip_prefix(' ').unwrap_or(body);
                let change = match prefix {
                    "[wait]" => parse_wait_start(body, line_no)?,
                    _ => {
                        let wait_id = frozen_wait_id.ok_or_else(||
                            invalid(line_no, "no semantic wait exists in the frozen context"))?;
                        match prefix {
                            "[wait:continue]" => parse_wait_continue(body, line_no, wait_id)?,
                            "[wait:end]" => parse_wait_end(body, line_no, wait_id)?,
                            "[wait:replace]" => parse_wait_replace(body, line_no, wait_id)?,
                            _ => unreachable!("wait_prefix returns only known controls"),
                        }
                    }
                };
                wait_controls += 1;
                result.controls.push(OutputControl {
                    action_id: EntityId::new(),
                    spans: vec![span],
                    action: OutputAction::SemanticWait { change },
                });
                continue;
            }
            let (header, value) = line
                .split_once(']')
                .ok_or_else(|| invalid(line_no, "missing ]"))?;
            let value = value.strip_prefix(' ').unwrap_or(value);
            let action = if let Some(target) = header.strip_prefix("[reply:") {
                // The reply id is an OPAQUE platform identity. It is taken
                // verbatim to the end of the line and resolved by the plugin
                // that owns the kind; the core never parses it. SPEC §16.
                let (kind, id) = target
                    .split_once(':')
                    .ok_or_else(|| invalid(line_no, "reply target must include kind and id"))?;
                let id = id.trim();
                if kind.is_empty() || id.is_empty() {
                    return Err(invalid(line_no, "reply target must include kind and id"));
                }
                let plugin = channels
                    .plugin_for_kind(kind)
                    .ok_or_else(|| invalid(line_no, "unknown reply target"))?;
                let target = plugin
                    .resolve_reply(kind, id)
                    .ok_or_else(|| invalid(line_no, "reply id is not resolvable on this channel"))?;
                let text = if value.is_empty() {
                    String::new()
                } else {
                    format!("{value}{}", &chunk[line.len()..])
                };
                result.text.push_str(&text);
                reply = Some(result.controls.len());
                OutputAction::Reply { target, text }
            } else if let Some(change) =
                identity.parse_control(header.trim_start_matches('['), value)
            {
                if !identity_fields.insert(std::mem::discriminant(&change)) {
                    return Err(invalid(line_no, "duplicate identity field in one result"));
                }
                if value.trim().is_empty() {
                    return Err(invalid(line_no, "empty identity change"));
                }
                OutputAction::Identity { change }
            } else {
                if value.trim().is_empty() {
                    return Err(invalid(line_no, "empty action"));
                }
                match header {
                    "[remember" => OutputAction::Remember {
                        text: value.to_owned(),
                    },
                    "[dispose" => {
                        OutputAction::Dispose {
                            spec: crate::authored_controls::parse_disposition(value, line_no)?,
                        }
                    }
                    "[obligation" => {
                        OutputAction::Obligation {
                            spec: crate::authored_controls::parse_obligation(value, line_no)?,
                        }
                    }
                    "[dialogue" => {
                        OutputAction::Dialogue {
                            spec: crate::authored_controls::parse_dialogue(value, line_no)?,
                        }
                    }
                    "[dnd" => {
                        OutputAction::Dnd {
                            spec: crate::authored_controls::parse_dnd(value, line_no)?,
                        }
                    }
                    "[steer" => {
                        OutputAction::Steer {
                            spec: crate::authored_controls::parse_steer(value, line_no)?,
                        }
                    }
                    "[delegate" => {
                        let spec: shenko_types::output::ModelChildSpec = serde_json::from_str(value)
                            .map_err(|_| invalid(line_no, "model delegation requires goal, system_prompt, model and max_tokens JSON"))?;
                        if spec.goal.trim().is_empty() || spec.system_prompt.trim().is_empty()
                            || spec.goal.len() > 64 * 1024 || spec.system_prompt.len() > 64 * 1024
                            || spec.model.trim().is_empty() || spec.model.len() > 512
                            || spec.model.chars().any(char::is_control) || spec.max_tokens == 0
                        {
                            return Err(invalid(line_no, "invalid model child goal/prompt/model/budget"));
                        }
                        OutputAction::DelegateModel { spec }
                    }
                    _ if header.starts_with("[delegate:") => {
                        let spec = header.trim_start_matches("[delegate:");
                        let (kind, version) = spec.rsplit_once(':').ok_or_else(|| {
                            invalid(line_no, "native delegation requires kind and version")
                        })?;
                        let valid_kind = !kind.is_empty()
                            && kind != "delegation" && kind != "model.child"
                            && kind.bytes().all(|c| {
                                c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-')
                            });
                        if !valid_kind {
                            return Err(invalid(line_no, "invalid native definition kind"));
                        }
                        let definition_version = version
                            .parse::<u32>()
                            .ok()
                            .filter(|v| *v > 0)
                            .ok_or_else(|| invalid(line_no, "invalid native definition version"))?;
                        let input = serde_json::from_str(value).map_err(|_| {
                            invalid(line_no, "native delegation input must be one JSON value")
                        })?;
                        OutputAction::Delegate {
                            text: value.to_owned(),
                            definition: Some(shenko_types::task::NativeTaskDefinition {
                                kind: kind.to_owned(),
                                definition_version,
                                input,
                            }),
                        }
                    }
                    _ => return Err(invalid(line_no, "unknown control")),
                }
            };
            result.controls.push(OutputControl {
                action_id: EntityId::new(),
                spans: vec![span],
                action,
            });
        } else {
            result.text.push_str(chunk);
            if let Some(index) = reply {
                let control = &mut result.controls[index];
                if let OutputAction::Reply { text, .. } = &mut control.action {
                    text.push_str(chunk);
                }
                control.spans.push(span);
            }
        }
    }
    Ok(result)
}

