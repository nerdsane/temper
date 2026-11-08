//! What the policies test of a sender.

use super::*;

impl Tests {
    pub(super) fn read(policies: &[(Effect, Json)]) -> Self {
        let mut tests = Tests::default();
        for (_, policy) in policies {
            tests.policy(policy);
        }
        tests.kinds.sort();
        tests.kinds.dedup();
        tests
    }

    fn policy(&mut self, policy: &Json) {
        let mut reads = BTreeSet::new();
        let scope = &policy["principal"];
        match scope["op"].as_str() {
            Some("==") => self.principal_is_entity(&scope["entity"], &mut reads),
            Some("is") => self.principal_is_kind(scope["entity_type"].as_str(), &mut reads),
            _ => {}
        }
        self.scopes.push(reads.clone());
        self.walk(&policy["conditions"], &mut reads);
        self.reads.push(reads);
    }

    /// Note what the policies some question may apply compare a sender with.
    pub(super) fn ask(&mut self, policies: &[(Effect, Json)], candidates: &[Vec<usize>]) {
        let mut asked = Tests::default();
        let some: BTreeSet<usize> = candidates.iter().flatten().copied().collect();
        for at in some {
            asked.policy(&policies[at].1);
        }
        asked.kinds.sort();
        asked.kinds.dedup();
        self.asked_kinds = asked.kinds;
        self.asked_literals = asked.literals;
    }

    /// The tests `expr` makes.
    pub(super) fn of(expr: &Json) -> BTreeSet<Test> {
        let mut reads = BTreeSet::new();
        Tests::default().walk(expr, &mut reads);
        reads
    }

    fn principal_is_kind(&mut self, kind: Option<&str>, reads: &mut BTreeSet<Test>) {
        // Someone outside is never the kernel's `System`.
        if let Some(kind) = kind.filter(|kind| *kind != "System") {
            self.kinds.push(kind.to_string());
            reads.insert(Test::Kind);
        }
    }

    fn principal_is_entity(&mut self, entity: &Json, reads: &mut BTreeSet<Test>) {
        let Some((kind, id)) = entity_parts(entity) else {
            return;
        };
        self.principal_is_kind(Some(kind), reads);
        let id_attr = (Place::Principal, "id".to_string());
        self.literal(&id_attr, Json::from(id));
        reads.insert(Test::Named(id_attr.0, id_attr.1));
    }

    fn literal(&mut self, of: &(Place, String), value: Json) {
        let literals = self.literals.entry(of.clone()).or_default();
        if !literals.contains(&value) {
            literals.push(value);
        }
    }

    fn walk(&mut self, expr: &Json, reads: &mut BTreeSet<Test>) {
        let Some((op, arg)) = single(expr) else {
            match expr {
                Json::Object(object) => object.values().for_each(|child| self.walk(child, reads)),
                Json::Array(items) => items.iter().for_each(|child| self.walk(child, reads)),
                _ => {}
            }
            return;
        };
        if op == "Value" {
            return;
        }
        if let Some(of) = named(op, arg) {
            reads.insert(Test::Named(of.0, of.1));
        }
        let (left, right) = (&arg["left"], &arg["right"]);
        let side = |expr: &Json| single(expr).and_then(|(op, arg)| named(op, arg));
        match op {
            "==" | "!=" => {
                for (subject, other) in [(left, right), (right, left)] {
                    let Some(other) = value(other) else { continue };
                    if let Some(of) = side(subject) {
                        self.literal(&of, other.clone());
                    } else if place(subject) == Some(Place::Principal) {
                        self.principal_is_entity(other, reads);
                    }
                }
            }
            "in" => {
                if let (Some(of), Some(("Set", Json::Array(members)))) = (side(left), single(right))
                {
                    for member in members.iter().filter_map(value) {
                        self.literal(&of, member.clone());
                    }
                }
            }
            "contains" => {
                if let (Some(("Set", Json::Array(members))), Some(of)) = (single(left), side(right))
                {
                    for member in members.iter().filter_map(value) {
                        self.literal(&of, member.clone());
                    }
                }
            }
            "is" if place(left) == Some(Place::Principal) => {
                self.principal_is_kind(arg["entity_type"].as_str(), reads);
            }
            "like" => {
                if let Some(of) = side(left) {
                    reads.insert(Test::Like(of.0, of.1, arg["pattern"].to_string()));
                }
            }
            _ => {}
        }
        self.walk(arg, reads);
    }

    /// Whether `profile` leaves `test` open.
    pub(super) fn open(&self, profile: &Profile, test: &Test) -> bool {
        match test {
            Test::Kind => profile.kind.is_none(),
            Test::Named(place, name) => !facts(profile, *place).contains_key(name),
            Test::Like(place, name, pattern) => matches!(
                facts(profile, *place).get(name),
                Some(Fact::Other { like, string, .. })
                    if *string != Some(false) && !like.contains_key(pattern)
            ),
        }
    }
}
