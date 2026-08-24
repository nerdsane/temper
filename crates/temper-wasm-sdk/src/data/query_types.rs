use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Stable query ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderV1 {
    /// Order by one canonical CSDL property.
    Property {
        /// Canonical property name.
        field: String,
        /// Requested sort direction.
        direction: OrderDirectionV1,
    },
    /// Order by the host-owned entity commit sequence.
    EntityCommitSequence {
        /// Requested sort direction.
        direction: OrderDirectionV1,
    },
}

impl OrderV1 {
    /// Construct property ordering while preserving the original v1 wire shape.
    pub fn property(field: impl Into<String>, direction: OrderDirectionV1) -> Self {
        Self::Property {
            field: field.into(),
            direction,
        }
    }

    /// Return the requested direction for either order target.
    pub const fn direction(&self) -> OrderDirectionV1 {
        match self {
            Self::Property { direction, .. } | Self::EntityCommitSequence { direction } => {
                *direction
            }
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum OrderSerialize<'a> {
    Property {
        field: &'a str,
        direction: OrderDirectionV1,
    },
    EntityCommitSequence {
        kind: &'static str,
        direction: OrderDirectionV1,
    },
}

impl Serialize for OrderV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Property { field, direction } => OrderSerialize::Property {
                field,
                direction: *direction,
            }
            .serialize(serializer),
            Self::EntityCommitSequence { direction } => OrderSerialize::EntityCommitSequence {
                kind: "entity_commit_sequence",
                direction: *direction,
            }
            .serialize(serializer),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OrderDeserialize {
    Property(OrderProperty),
    EntityCommitSequence(OrderEntityCommitSequence),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OrderProperty {
    field: String,
    direction: OrderDirectionV1,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OrderEntityCommitSequence {
    kind: OrderEntityCommitSequenceKind,
    direction: OrderDirectionV1,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum OrderEntityCommitSequenceKind {
    EntityCommitSequence,
}

impl<'de> Deserialize<'de> for OrderV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match OrderDeserialize::deserialize(deserializer)? {
            OrderDeserialize::Property(order) => Ok(Self::Property {
                field: order.field,
                direction: order.direction,
            }),
            OrderDeserialize::EntityCommitSequence(order) => {
                let OrderEntityCommitSequenceKind::EntityCommitSequence = order.kind;
                Ok(Self::EntityCommitSequence {
                    direction: order.direction,
                })
            }
        }
    }
}

/// Query sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderDirectionV1 {
    /// Ascending, with nulls last.
    Asc,
    /// Descending, with nulls first.
    Desc,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_order_preserves_the_original_v1_wire_shape() {
        let order = OrderV1::property("CreatedAt", OrderDirectionV1::Asc);
        assert_eq!(
            serde_json::to_value(&order).unwrap(),
            serde_json::json!({"field": "CreatedAt", "direction": "asc"})
        );
        assert_eq!(
            serde_json::from_value::<OrderV1>(serde_json::json!({
                "field": "CreatedAt",
                "direction": "asc"
            }))
            .unwrap(),
            order
        );
    }

    #[test]
    fn commit_sequence_order_has_no_caller_selected_property() {
        let order = OrderV1::EntityCommitSequence {
            direction: OrderDirectionV1::Desc,
        };
        let value = serde_json::to_value(&order).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "entity_commit_sequence",
                "direction": "desc"
            })
        );
        assert!(value.get("field").is_none());
        assert_eq!(serde_json::from_value::<OrderV1>(value).unwrap(), order);
    }
}

/// Bounded page request. Cursors are opaque host output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageV1 {
    /// Maximum values returned in this page.
    pub limit: u32,
    /// Opaque cursor returned by an identical prior query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}
