use std::cmp::Ordering;

pub(super) fn compare_decimal(left: &str, right: &str) -> Option<Ordering> {
    let left_negative = left.starts_with('-');
    let right_negative = right.starts_with('-');
    if left_negative != right_negative {
        return Some(if left_negative {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let magnitude = compare_magnitude(left.trim_start_matches('-'), right.trim_start_matches('-'))?;
    Some(if left_negative {
        magnitude.reverse()
    } else {
        magnitude
    })
}

fn compare_magnitude(left: &str, right: &str) -> Option<Ordering> {
    let (left_whole, left_fraction) = left.split_once('.').unwrap_or((left, ""));
    let (right_whole, right_fraction) = right.split_once('.').unwrap_or((right, ""));
    if !left_whole.bytes().all(|byte| byte.is_ascii_digit())
        || !right_whole.bytes().all(|byte| byte.is_ascii_digit())
        || !left_fraction.bytes().all(|byte| byte.is_ascii_digit())
        || !right_fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    match left_whole.len().cmp(&right_whole.len()) {
        Ordering::Equal => {}
        ordering => return Some(ordering),
    }
    match left_whole.cmp(right_whole) {
        Ordering::Equal => {}
        ordering => return Some(ordering),
    }
    let width = left_fraction.len().max(right_fraction.len());
    Some(
        left_fraction
            .bytes()
            .chain(std::iter::repeat(b'0'))
            .take(width)
            .cmp(
                right_fraction
                    .bytes()
                    .chain(std::iter::repeat(b'0'))
                    .take(width),
            ),
    )
}
