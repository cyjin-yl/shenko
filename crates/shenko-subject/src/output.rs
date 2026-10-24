//! Decode once at the model-result boundary; consumers never inspect raw text.
//! Controls are whole unindented lines outside Markdown fences/quotes. There is
//! no natural-language trigger, legacy alias, or raw-publication fallback.
use shenko_identity::DefaultIdentityPolicy;
use shenko_types::{
    error::{ErrorScope, ProtocolError, RetryStrategy, ShenkoError},
    ids::EntityId,
    object::ObjectRef,
    output::*,
};

pub const OUTPUT_CONTRACT: &str = "控制动作只在独立、无缩进、非引用/代码块的一行生效：\n\
[reply:group:群号] 正文 或 [reply:private:QQ号] 正文，后续普通行续接正文；\n\
[remember] 要保存的内容；[delegate] 有限子任务的完整目标与提示。\n\
身份控制由身份能力说明提供。控制行不是可发布正文；引用/演示请放代码块或引用块。\n\
只支持此协议；[memory]、dispatch: 等旧伪协议不执行。";

fn invalid(line: u64, reason: &str) -> ShenkoError {
    ProtocolError::new(
        "CONTROL_PROTOCOL_INVALID",
        ErrorScope::Request,
        RetryStrategy::NewRevision,
    )
    .with_details(serde_json::json!({"line":line,"reason":reason,"protocol":1}))
    .into()
}

/// The caller has already archived the complete original output at raw_ref.
pub fn parse_output(
    step_id: EntityId,
    raw_ref: ObjectRef,
    raw: &str,
) -> Result<ParsedOutput, ShenkoError> {
    let mut result = ParsedOutput {
        schema_version: 1,
        step_id,
        raw_ref,
        text: String::new(),
        controls: vec![],
    };
    let mut offset = 0;
    let mut reply: Option<usize> = None;
    let mut fence: Option<(u8, usize)> = None;
    let mut identity_fields = std::collections::BTreeSet::new();
    for (index, chunk) in raw.split_inclusive('\n').enumerate() {
        let line_no = index as u64 + 1;
        let line = chunk.trim_end_matches(['\r', '\n']);
        let span = OutputSpan {
            start: offset,
            end: offset + chunk.len(),
            line: line_no,
        };
        offset += chunk.len();
        let indentation = line.len() - line.trim_start_matches(' ').len();
        let trimmed = line.trim_start_matches(' ');
        let mark = trimmed.as_bytes().first().copied();
        let count = mark.map_or(0, |m| trimmed.bytes().take_while(|b| *b == m).count());
        let was_fenced = fence.is_some();
        if indentation <= 3 && matches!(mark, Some(b'`' | b'~')) && count >= 3 {
            match fence {
                None => fence = Some((mark.unwrap(), count)),
                Some((m, n))
                    if mark == Some(m) && count >= n && trimmed[count..].trim().is_empty() =>
                {
                    fence = None
                }
                _ => {}
            }
        }
        let active = !was_fenced && fence.is_none() && indentation == 0;
        let control = active
            && [
                "[reply:",
                "[identity:",
                "[remember]",
                "[delegate]",
                "[memory]",
                "dispatch:",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix));
        if control {
            if line.starts_with("[memory]") || line.starts_with("dispatch:") {
                return Err(invalid(
                    line_no,
                    "obsolete control protocol; use [remember] / [delegate]",
                ));
            }
            let (header, value) = line
                .split_once(']')
                .ok_or_else(|| invalid(line_no, "missing ]"))?;
            let value = value.strip_prefix(' ').unwrap_or(value);
            let action = if let Some(target) = header.strip_prefix("[reply:") {
                let (kind, id) = target
                    .split_once(':')
                    .ok_or_else(|| invalid(line_no, "reply target must include kind and id"))?;
                let id = id
                    .parse::<u64>()
                    .ok()
                    .filter(|id| *id > 0)
                    .ok_or_else(|| invalid(line_no, "reply id must be positive"))?;
                let target = match kind {
                    "group" => PublicationTarget::QqGroup(id),
                    "private" => PublicationTarget::QqPrivate(id),
                    _ => return Err(invalid(line_no, "unknown reply target")),
                };
                let text = if value.is_empty() {
                    String::new()
                } else {
                    format!("{value}{}", &chunk[line.len()..])
                };
                result.text.push_str(&text);
                reply = Some(result.controls.len());
                OutputAction::Reply { target, text }
            } else if let Some(field) = header.strip_prefix("[identity:") {
                if !identity_fields.insert(field.to_owned()) {
                    return Err(invalid(line_no, "duplicate identity field in one result"));
                }
                let change = DefaultIdentityPolicy::parse_change(field, value)
                    .ok_or_else(|| invalid(line_no, "unknown identity field"))?;
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
                    "[delegate" => OutputAction::Delegate {
                        text: value.to_owned(),
                    },
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
