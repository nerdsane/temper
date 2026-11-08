//! The order families of a model that holds several records of a type.

use std::collections::BTreeMap;

use super::Order;

impl Order {
    /// These families for every record's name: copies keep one family, since
    /// only differences within it count.
    pub(in crate::composite::joint) fn fan(&mut self, copies: &BTreeMap<String, Vec<String>>) {
        for family in &mut self.families {
            super::super::copies::fan_set(&mut family.fields, copies);
            for (entity, action, param) in family.params.clone() {
                for name in copies.get(&entity).into_iter().flatten().skip(1) {
                    family
                        .params
                        .insert((name.clone(), action.clone(), param.clone()));
                }
            }
        }
        super::super::copies::fan_moves(&mut self.compared, copies);
        super::super::copies::fan_set(&mut self.rising, copies);
    }
}
