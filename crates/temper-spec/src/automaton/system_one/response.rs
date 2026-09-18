//! Validate native TypeSafe answers before any assertion can accept them.

use serde_json::Value;

use super::decimal::{SCALE, fixed, within_answer_range};
use super::{SystemOneGuard, SystemOneQuestion};

pub(super) fn validate(guard: &SystemOneGuard, response: &Value) -> Result<(), String> {
    if response
        .get("model")
        .and_then(Value::as_str)
        .is_none_or(|s| s.trim().is_empty())
    {
        return Err("system_one response requires a model identity".into());
    }
    let answers = response
        .get("answers")
        .and_then(Value::as_object)
        .ok_or("system_one response requires an answers object")?;
    if answers.len() != guard.questions.len()
        || answers.keys().any(|k| !guard.questions.contains_key(k))
    {
        return Err("system_one response question IDs do not match the request".into());
    }
    for (name, question) in &guard.questions {
        validate_answer(question, &answers[name]).map_err(|e| format!("answer '{name}': {e}"))?;
    }
    Ok(())
}

fn validate_answer(question: &SystemOneQuestion, answer: &Value) -> Result<(), String> {
    let object = answer.as_object().ok_or("answer must be an object")?;
    if answer.get("type").and_then(Value::as_str) != Some(question.type_name()) {
        return Err("answer type does not match the question".into());
    }
    match question {
        SystemOneQuestion::Noul { .. } => {
            only_fields(object, &["type", "noul"])?;
            number(answer.get("noul"), 0, SCALE)?;
        }
        SystemOneQuestion::Choice { criteria, .. } => {
            only_fields(object, &["type", "choice", "probabilities", "confidence"])?;
            number(answer.get("confidence"), 0, SCALE)?;
            let choice = answer
                .get("choice")
                .and_then(Value::as_str)
                .ok_or("choice must be a string")?;
            if !criteria.contains_key(choice) {
                return Err("choice is not a declared option".into());
            }
            let keys: Vec<_> = criteria.keys().cloned().collect();
            let probabilities = probabilities(answer, &keys)?;
            if probabilities
                .iter()
                .any(|(name, p)| name != choice && *p > probabilities[choice])
            {
                return Err("choice is not a highest-probability option".into());
            }
        }
        SystemOneQuestion::Score { criteria, .. } => {
            only_fields(
                object,
                &["type", "score", "probabilities", "confidence", "legend"],
            )?;
            number(answer.get("confidence"), 0, SCALE)?;
            let max = (criteria.len() - 1) as i128 * SCALE;
            let score = number(answer.get("score"), 0, max)?;
            let keys: Vec<_> = (0..criteria.len()).map(|i| i.to_string()).collect();
            let distribution = probabilities(answer, &keys)?;
            let legend = answer
                .get("legend")
                .and_then(Value::as_object)
                .ok_or("score requires a legend object")?;
            if legend.len() != keys.len()
                || keys
                    .iter()
                    .any(|k| !legend.get(k).is_some_and(Value::is_string))
            {
                return Err("score legend does not match rubric levels".into());
            }
            for (index, description) in criteria.iter().enumerate() {
                if let Some(expected) = description.as_str()
                    && legend[&index.to_string()].as_str() != Some(expected)
                {
                    return Err(
                        "score legend description does not match the declared rubric".into(),
                    );
                }
            }
            let expected: i128 = (0..criteria.len())
                .map(|i| i as i128 * distribution[&i.to_string()])
                .sum();
            if (score - expected).abs() > 1_000 * criteria.len() as i128 {
                return Err("score does not match its probability-weighted rubric".into());
            }
        }
    }
    Ok(())
}

fn probabilities(
    answer: &Value,
    keys: &[String],
) -> Result<std::collections::BTreeMap<String, i128>, String> {
    let values = answer
        .get("probabilities")
        .and_then(Value::as_object)
        .ok_or("answer requires a probabilities object")?;
    if values.len() != keys.len() || keys.iter().any(|k| !values.contains_key(k)) {
        return Err("probabilities do not match declared options or levels".into());
    }
    let distribution = values
        .iter()
        .map(|(k, v)| Ok((k.clone(), number(Some(v), 0, SCALE)?)))
        .collect::<Result<std::collections::BTreeMap<_, _>, String>>()?;
    // The API returns decimal probabilities; tolerate serialization/rounding up
    // to one part per million while retaining deterministic integer arithmetic.
    if (distribution.values().sum::<i128>() - SCALE).abs() > 1_000 {
        return Err("probabilities must sum to one within 0.000001".into());
    }
    Ok(distribution)
}

fn only_fields(object: &serde_json::Map<String, Value>, fields: &[&str]) -> Result<(), String> {
    if object.len() != fields.len() || object.keys().any(|k| !fields.contains(&k.as_str())) {
        return Err("answer has missing or unsupported fields".into());
    }
    Ok(())
}

pub(super) fn number(value: Option<&Value>, min: i128, max: i128) -> Result<i128, String> {
    let number = value
        .and_then(Value::as_number)
        .ok_or("expected a numeric answer")?;
    let normalized = fixed(&number.to_string())?;
    if normalized < min
        || normalized > max
        || !within_answer_range(&number.to_string(), max / SCALE)
    {
        return Err("numeric answer is outside its declared range".into());
    }
    Ok(normalized)
}
