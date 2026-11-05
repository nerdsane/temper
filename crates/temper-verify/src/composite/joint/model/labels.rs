//! Move labels with symbol parameters renamed without changing literals.

/// A move's words, built only on a thread that wants them: a walk names its
/// moves when it prints them, not for each of the states it passes through.
macro_rules! words {
    ($model:expr, $($text:tt)*) => {
        if $model.expansion.words() {
            format!($($text)*)
        } else {
            String::new()
        }
    };
}

use super::step::Move;
#[cfg(test)]
use crate::model::ModelString;
#[cfg(test)]
use crate::model::TemperModelAction;
use crate::model::{Fields, ParamValue, Renames};

/// Render the move with renamed symbol parameters, preserving concrete literals.
pub(super) fn rename_label(text: &str, mv: &Move, renamed: &Renames) -> String {
    let rename_params = |params: &mut Fields<ParamValue>| {
        for value in params.values_mut() {
            if let ParamValue::Str(s) = value
                && let Some(new) = renamed.get(s)
            {
                *s = *new;
            }
        }
    };
    let (before, after) = match mv {
        Move::Take(entity, action, _)
        | Move::Send(entity, action, _)
        | Move::Reaction(entity, action, _) => {
            let mut new = action.clone();
            rename_params(&mut new.params);
            (format!("{entity}.{action}"), format!("{entity}.{new}"))
        }
        Move::Answer(call, action) => {
            let mut new = action.clone();
            rename_params(&mut new.params);
            (
                format!("{}.{action}", call.to),
                format!("{}.{new}", call.to),
            )
        }
        Move::Timer(..) | Move::Fail(..) | Move::Lost(..) | Move::Silent(..) => {
            return text.to_string();
        }
        Move::Read(dispatch) | Move::Arrive(dispatch) => {
            let mut new = (**dispatch).clone();
            rename_params(&mut new.params);
            (dispatch.to_string(), new.to_string())
        }
    };
    format!(
        "{after}{}",
        text.strip_prefix(&before)
            .expect("label starts with its move")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renaming_a_move_preserves_a_literal_that_spells_a_symbol() {
        let action = TemperModelAction {
            name: "Set".into(),
            target_state: None,
            params: Fields::from([
                ("a", ParamValue::Str(ModelString::from("#2"))),
                ("b", ParamValue::Str(ModelString::Symbol(2))),
            ]),
        };
        let text = format!("T.{action} sent");
        let label = rename_label(
            &text,
            &Move::Send("T".into(), action, 0),
            &Renames::from_pairs([(ModelString::Symbol(2), ModelString::Symbol(1))]),
        );
        assert_eq!(label, "T.Set(a='#2', b='#1') sent");
    }
}
