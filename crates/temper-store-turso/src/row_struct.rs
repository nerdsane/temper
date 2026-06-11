//! Internal macro for mapping positional libsql row columns to struct
//! literals, replacing the hand-rolled
//! `row.get::<T>(i).map_err(storage_error)?` chains in row-mapping code.

/// Build a struct literal from positional libsql row columns.
///
/// Each field takes one of two forms:
///
/// - `name: idx as Type` — expands to the crate-standard column read
///   `row.get::<Type>(idx).map_err(storage_error)?`
/// - `name: expression` — passed through verbatim, for columns that need
///   post-processing (bool from i64, integer narrowing, JSON parsing, …)
///
/// Fields expand in written order with the written indices and types — the
/// macro is pure syntax over the repetitive get + map_err chains.
///
/// Invoke with braces (`row_struct! { row, Name { … } }`) so rustfmt leaves
/// the field table formatting alone.
macro_rules! row_struct {
    ($row:expr, $name:ident { $($body:tt)* }) => {
        row_struct!(@field $row, $name, () $($body)*)
    };
    (@field $row:expr, $name:ident, ($($out:tt)*)) => {
        $name { $($out)* }
    };
    (@field $row:expr, $name:ident, ($($out:tt)*) $field:ident : $idx:literal as $ty:ty, $($rest:tt)*) => {
        row_struct!(@field $row, $name, ($($out)* $field: $row
            .get::<$ty>($idx)
            .map_err(temper_runtime::persistence::storage_error)?,) $($rest)*)
    };
    (@field $row:expr, $name:ident, ($($out:tt)*) $field:ident : $idx:literal as $ty:ty) => {
        row_struct!(@field $row, $name, ($($out)*) $field: $idx as $ty,)
    };
    (@field $row:expr, $name:ident, ($($out:tt)*) $field:ident : $value:expr, $($rest:tt)*) => {
        row_struct!(@field $row, $name, ($($out)* $field: $value,) $($rest)*)
    };
    (@field $row:expr, $name:ident, ($($out:tt)*) $field:ident : $value:expr) => {
        row_struct!(@field $row, $name, ($($out)*) $field: $value,)
    };
}
pub(crate) use row_struct;
