use std::collections::HashMap;

use super::{StepResult, StepStatus};

pub(crate) fn evaluate(
    condition: Option<&str>,
    results: &HashMap<String, StepResult>,
) -> Result<bool, String> {
    let Some(condition) = condition.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(true);
    };

    eval_or(condition, results)
}

fn eval_or(expr: &str, results: &HashMap<String, StepResult>) -> Result<bool, String> {
    let parts = split_top_level(expr, "||");
    if parts.len() > 1 {
        for part in parts {
            if eval_and(part, results)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }

    eval_and(expr, results)
}

fn eval_and(expr: &str, results: &HashMap<String, StepResult>) -> Result<bool, String> {
    let parts = split_top_level(expr, "&&");
    if parts.len() > 1 {
        for part in parts {
            if !eval_comparison(part, results)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }

    eval_comparison(expr, results)
}

fn eval_comparison(expr: &str, results: &HashMap<String, StepResult>) -> Result<bool, String> {
    for op in ["==", "!=", ">", "<"] {
        if let Some((left, right)) = split_once_operator(expr, op) {
            let left = value_of(left.trim(), results)?;
            let right = value_of(right.trim(), results)?;
            return match op {
                "==" => Ok(left == right),
                "!=" => Ok(left != right),
                ">" => Ok(left > right),
                "<" => Ok(left < right),
                _ => unreachable!(),
            };
        }
    }

    Err(format!("unsupported condition expression '{expr}'"))
}

fn split_once_operator<'a>(expr: &'a str, op: &str) -> Option<(&'a str, &'a str)> {
    let mut in_quote = None;
    let bytes = expr.as_bytes();
    let op_bytes = op.as_bytes();
    let mut i = 0;

    while i + op_bytes.len() <= bytes.len() {
        let ch = bytes[i] as char;
        match (ch, in_quote) {
            ('"' | '\'', None) => in_quote = Some(ch),
            (quote, Some(open)) if quote == open => in_quote = None,
            _ => {}
        }

        if in_quote.is_none() && &bytes[i..i + op_bytes.len()] == op_bytes {
            return Some((&expr[..i], &expr[i + op_bytes.len()..]));
        }
        i += 1;
    }

    None
}

fn split_top_level<'a>(expr: &'a str, delimiter: &str) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut in_quote = None;
    let bytes = expr.as_bytes();
    let delimiter_bytes = delimiter.as_bytes();
    let mut start = 0;
    let mut i = 0;

    while i + delimiter_bytes.len() <= bytes.len() {
        let ch = bytes[i] as char;
        match (ch, in_quote) {
            ('"' | '\'', None) => in_quote = Some(ch),
            (quote, Some(open)) if quote == open => in_quote = None,
            _ => {}
        }

        if in_quote.is_none() && &bytes[i..i + delimiter_bytes.len()] == delimiter_bytes {
            parts.push(expr[start..i].trim());
            i += delimiter_bytes.len();
            start = i;
            continue;
        }
        i += 1;
    }

    parts.push(expr[start..].trim());
    parts
}

fn value_of(token: &str, results: &HashMap<String, StepResult>) -> Result<String, String> {
    if let Some(value) = quoted_literal(token) {
        return Ok(value.to_string());
    }

    resolve_reference(token, results)
}

fn quoted_literal(token: &str) -> Option<&str> {
    let bytes = token.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let first = bytes[0] as char;
    let last = bytes[bytes.len() - 1] as char;
    ((first == '"' || first == '\'') && first == last).then_some(&token[1..token.len() - 1])
}

pub(crate) fn resolve_reference(
    reference: &str,
    results: &HashMap<String, StepResult>,
) -> Result<String, String> {
    let Some(reference) = reference.strip_prefix('$') else {
        return Err(format!("unsupported condition operand '{reference}'"));
    };
    let Some((step_id, field)) = reference.rsplit_once('.') else {
        return Err(format!("invalid reference '${reference}'"));
    };
    let Some(result) = results.get(step_id) else {
        return Err(format!("reference '${reference}' is not ready"));
    };

    match field {
        "output" => result
            .output
            .clone()
            .ok_or_else(|| format!("reference '${reference}' has no output")),
        "status" => Ok(result.status.as_str().to_string()),
        _ => Err(format!("unsupported reference field '{field}'")),
    }
}

impl StepStatus {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            StepStatus::Success => "success",
            StepStatus::Failed => "failed",
            StepStatus::Skipped => "skipped",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(status: StepStatus, output: Option<&str>) -> StepResult {
        StepResult {
            step_id: "a".to_string(),
            status,
            output: output.map(str::to_string),
            error: None,
        }
    }

    #[test]
    fn evaluates_status_and_output_checks() {
        let mut results = HashMap::new();
        results.insert("a".to_string(), result(StepStatus::Success, Some("42")));

        assert!(evaluate(Some("$a.status == 'success'"), &results).unwrap());
        assert!(evaluate(
            Some("$a.output == \"42\" && $a.status != 'failed'"),
            &results
        )
        .unwrap());
        assert!(!evaluate(
            Some("$a.output == 'nope' || $a.status == 'failed'"),
            &results
        )
        .unwrap());
    }
}
