use super::*;

impl Tests {
    /// The profiles that tell `profile` apart on `test`: between them,
    /// every value it may have, a literal no question's policy compares with
    /// standing with the other values.
    pub(super) fn split(&self, profile: &Profile, test: &Test) -> Vec<Profile> {
        let with = |set: &dyn Fn(&mut Profile)| {
            let mut next = profile.clone();
            set(&mut next);
            next
        };
        match test {
            Test::Kind => {
                let unlike: Vec<Json> = self
                    .kinds
                    .iter()
                    .map(|kind| Json::from(kind.as_str()))
                    .collect();
                let mut facts: Vec<Fact> = self
                    .asked_kinds
                    .iter()
                    .map(|kind| Fact::Is(Json::from(kind.as_str())))
                    .collect();
                facts.push(Fact::Other {
                    unlike,
                    like: BTreeMap::new(),
                    string: None,
                });
                facts
                    .into_iter()
                    .map(|fact| with(&|p: &mut Profile| p.kind = Some(fact.clone())))
                    .collect()
            }
            Test::Named(place, name) => {
                let of = (*place, name.clone());
                let literals = self.literals.get(&of).cloned().unwrap_or_default();
                // A principal always has its id; the engine sets both names.
                let always =
                    *place == Place::Principal && matches!(name.as_str(), "id" | "accountId");
                let mut facts: Vec<Fact> = Vec::new();
                if !always {
                    facts.push(Fact::Absent);
                }
                let asked = self.asked_literals.get(&of);
                facts.extend(asked.into_iter().flatten().cloned().map(Fact::Is));
                facts.push(Fact::Other {
                    unlike: literals,
                    like: BTreeMap::new(),
                    string: None,
                });
                facts
                    .into_iter()
                    .map(|fact| {
                        with(&|p: &mut Profile| {
                            facts_mut(p, *place).insert(name.clone(), fact.clone());
                        })
                    })
                    .collect()
            }
            Test::Like(place, name, pattern) => [Some(true), Some(false), None]
                .into_iter()
                .filter(|matches| {
                    matches.is_some()
                        || matches!(
                            facts(profile, *place).get(name),
                            Some(Fact::Other { string: None, .. })
                        )
                })
                .map(|matches| {
                    with(&|p: &mut Profile| {
                        if let Some(Fact::Other { like, string, .. }) =
                            facts_mut(p, *place).get_mut(name)
                        {
                            // Non-strings error for every pattern, including under !.
                            *string = Some(matches.is_some());
                            if let Some(matches) = matches {
                                like.insert(pattern.clone(), matches);
                            }
                        }
                    })
                })
                .collect(),
        }
    }
}

fn facts_mut(profile: &mut Profile, place: Place) -> &mut BTreeMap<String, Fact> {
    match place {
        Place::Principal => &mut profile.attrs,
        Place::Context => &mut profile.context,
    }
}
