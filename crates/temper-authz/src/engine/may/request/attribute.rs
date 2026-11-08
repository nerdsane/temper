//! What an expression reads of a value: an attribute, or whether it has one.

use std::borrow::Cow;

use super::{
    ANY, Asker, Attr, ERROR, FALSE, Fact, Request, Results, TRUE, TRUE_OR_FALSE, Val, made, read,
    record_of,
};

impl<'a> Request<'a> {
    /// A cause key's value, when the caller knows the cause: only the
    /// kernel sets these, so one the cause lacks is an error to read.
    fn cause_key(&self, of: &Val<'_>, key: &str) -> Option<Option<&'a Attr>> {
        match of {
            Val::Context => self.cause?.get(key),
            _ => None,
        }
    }

    pub(super) fn attribute<'e>(&self, of: &Val<'e>, attr: &str) -> Val<'e>
    where
        'a: 'e,
    {
        if let Some(value) = self.cause_key(of, attr) {
            return value.map_or(Val::Error, |a| self.known(a));
        }
        match (of, self.asker) {
            (Val::Principal, Asker::Service { name, .. }) => {
                Self::service_attribute(name, attr).map_or(Val::Error, made)
            }
            (Val::Context, Asker::Service { .. }) => {
                self.service_context(attr).unwrap_or(Val::Unknown)
            }
            (Val::Principal, _) => Self::fact_value(self.outside().and_then(|p| p.attrs.get(attr))),
            (Val::Context, _) if self.sender_key(attr).is_some() => {
                Self::fact_value(self.sender_key(attr))
            }
            // The kernel puts the record's id in its attributes under both
            // names (`build_authz_resource_attrs`).
            (Val::Resource, _) if matches!(attr, "id" | "Id") => self.known(&Attr::ResourceId),
            (Val::Resource, _) => self
                .record
                .get(attr)
                .map_or(Val::Unknown, |a| self.known(a)),
            (Val::Known(value), _) if record_of(value).is_some() => match value {
                Cow::Borrowed(value) => record_of(value)
                    .and_then(|record| record.get(attr))
                    .map_or(Val::Error, read),
                Cow::Owned(value) => record_of(value)
                    .and_then(|record| record.get(attr))
                    .map_or(Val::Error, |found| made(found.clone())),
            },
            (Val::Known(_) | Val::Action | Val::Error | Val::Same { .. }, _) => Val::Error,
            _ => Val::Unknown,
        }
    }

    /// The value of an attribute the kernel sets or the caller knows; the
    /// record's id is the caller's when it knows it.
    pub(super) fn known<'e>(&self, attr: &'e Attr) -> Val<'e>
    where
        'a: 'e,
    {
        match attr {
            Attr::Value(value) => read(value),
            Attr::ResourceId => match self.record.get("id") {
                Some(Attr::Value(value)) => read(value),
                Some(Attr::Same { name, unlike }) => Val::Same { name, unlike },
                _ => Val::ResourceId,
            },
            Attr::Same { name, unlike } => Val::Same { name, unlike },
            Attr::Unknown => Val::Unknown,
        }
    }

    pub(super) fn has(&self, of: &Val<'_>, attr: &str) -> Results {
        if let Some(value) = self.cause_key(of, attr) {
            return Results::of(value.is_some());
        }
        match (of, self.asker) {
            (Val::Principal, Asker::Service { name, .. }) => {
                Results::of(Self::service_attribute(name, attr).is_some())
            }
            (Val::Context, Asker::Service { .. }) => match self.service_context(attr) {
                Some(_) => TRUE,
                None => TRUE_OR_FALSE,
            },
            (Val::Principal, _) => match self.outside().and_then(|p| p.attrs.get(attr)) {
                Some(fact) => Results::of(*fact != Fact::Absent),
                None => TRUE_OR_FALSE,
            },
            (Val::Context, _) if self.sender_key(attr).is_some() => {
                Results::of(self.sender_key(attr) != Some(&Fact::Absent))
            }
            (Val::Other { .. }, _) => ANY,
            (Val::Known(value), _) if record_of(value).is_some() => {
                Results::of(record_of(value).is_some_and(|record| record.contains_key(attr)))
            }
            (Val::Action, _) => FALSE,
            (Val::Resource, _) if matches!(attr, "id" | "Id") => TRUE,
            (Val::Resource, _) if self.record.contains_key(attr) => TRUE,
            (Val::Known(_) | Val::Error | Val::ResourceId | Val::Same { .. }, _) => ERROR,
            (Val::Unknown, _) => ANY,
            _ => TRUE_OR_FALSE,
        }
    }
}
