//! Whether some sender may surely make a request, when the kind standing for
//! them holds some allowed and some refused.

use std::collections::BTreeMap;

use super::profile::{self, Profile};
use super::{Asker, Cause, Kind, May, Question, SendAnalysis};

impl SendAnalysis {
    /// Whether a request of `action` on a `resource_type` record is one the
    /// senders `asker` stands for can make. [`May::Yes`] means one of them is
    /// allowed it whatever the record holds, though others may be refused.
    /// A walk uses this for a request whose sender matters only for what
    /// follows it, on which all of them answer alike, so the one allowed may
    /// be the one sending. An answer open for each of them stays open.
    ///
    /// This only turns [`May::Maybe`] to `Yes`: whatever may be sent still is,
    /// and whatever none may send still is not. `reserve` is as for
    /// [`Self::outside_kinds`]; with no room to look closer, the answer stays
    /// open, which is never wrong.
    pub fn may_request(
        &self,
        asker: &Asker,
        action: &str,
        resource_type: &str,
        reserve: &dyn Fn(usize) -> Result<(), String>,
    ) -> May {
        let none = BTreeMap::new();
        let request = Cause::request();
        let may = self.may(asker, action, resource_type, &none, Some(&request));
        if may != May::Maybe {
            return may;
        }
        let surely = match asker {
            Asker::Service { .. } => false,
            Asker::OutsideAs(kind) => kind.every().any(|profile| {
                let one = Asker::OutsideAs(Kind::of(profile.clone()));
                self.may(&one, action, resource_type, &none, Some(&request)) == May::Yes
            }),
            Asker::Outside => self.someone(action, resource_type, reserve),
        };
        if surely { May::Yes } else { may }
    }

    /// The senders of the kind `asker` stands for who may surely make a
    /// request of `action` on a `resource_type` record, as a kind of their
    /// own, to name one of them: those of a known principal type when some
    /// are, as a person reads them more easily.
    pub fn allowed(&self, asker: &Asker, action: &str, resource_type: &str) -> Option<Kind> {
        let Asker::OutsideAs(kind) = asker else {
            return None;
        };
        let none = BTreeMap::new();
        let request = Cause::request();
        let allowed: Vec<Profile> = kind
            .every()
            .filter(|profile| {
                let one = Asker::OutsideAs(Kind::of((*profile).clone()));
                self.may(&one, action, resource_type, &none, Some(&request)) == May::Yes
            })
            .cloned()
            .collect();
        let typed = |profile: &&Profile| matches!(profile.kind, Some(profile::Fact::Is(_)));
        let known: Vec<Profile> = allowed.iter().filter(typed).cloned().collect();
        let names = if known.is_empty() { allowed } else { known };
        let mut names = names.into_iter();
        let first = Kind::of(names.next()?);
        Some(names.fold(first, |kind, profile| kind.join(Kind::of(profile))))
    }

    /// Whether some outside sender is allowed a request of `action` on a
    /// `resource_type` record, whatever the record holds: split the senders
    /// on this one question, and read each kind's answer.
    fn someone(
        &self,
        action: &str,
        resource_type: &str,
        reserve: &dyn Fn(usize) -> Result<(), String>,
    ) -> bool {
        let key = (action.to_string(), resource_type.to_string());
        let known = self
            .someone
            .lock()
            .ok()
            .and_then(|held| held.get(&key).copied());
        if let Some(known) = known {
            return known;
        }
        let question = Question {
            action,
            resource_type,
            cause: Cause::request(),
        };
        let Ok(kinds) = self.partition(std::slice::from_ref(&question), reserve) else {
            return false;
        };
        let found = kinds.iter().any(|kind| kind.answers[0] == May::Yes);
        if let Ok(mut held) = self.someone.lock() {
            held.insert(key, found);
        }
        found
    }
}
