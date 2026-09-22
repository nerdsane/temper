//! Fixed-point answer normalization: nine decimals, nearest, ties away from zero.

pub(super) const SCALE: i128 = 1_000_000_000;

pub(super) fn fixed(raw: &str) -> Result<i128, String> {
    let invalid = || format!("invalid or oversized system_one decimal '{raw}'");
    if raw.len() > 64 {
        return Err(invalid());
    }
    let (negative, raw) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let mut exponent_parts = raw.split(['e', 'E']);
    let mantissa = exponent_parts.next().ok_or_else(invalid)?;
    let exponent: i32 = exponent_parts
        .next()
        .map_or(Ok(0), |v| v.parse())
        .map_err(|_| invalid())?;
    if exponent_parts.next().is_some() || !(-10_000..=10_000).contains(&exponent) {
        return Err(invalid());
    }
    let mut decimals = mantissa.split('.');
    let whole = decimals.next().ok_or_else(invalid)?;
    let fraction = decimals.next().unwrap_or("");
    if decimals.next().is_some()
        || whole.is_empty()
        || !whole
            .chars()
            .chain(fraction.chars())
            .all(|c| c.is_ascii_digit())
    {
        return Err(invalid());
    }
    let digits = format!("{whole}{fraction}");
    let unscaled: i128 = digits.parse().map_err(|_| invalid())?;
    let power = 9 + exponent - i32::try_from(fraction.len()).map_err(|_| invalid())?;
    let magnitude = if power >= 0 {
        unscaled
            .checked_mul(10_i128.checked_pow(power as u32).ok_or_else(invalid)?)
            .ok_or_else(invalid)?
    } else if power <= -39 {
        0
    } else {
        let divisor = 10_i128.checked_pow((-power) as u32).ok_or_else(invalid)?;
        let quotient = unscaled / divisor;
        let remainder = unscaled % divisor;
        quotient + i128::from(remainder >= divisor / 2)
    };
    Ok(if negative { -magnitude } else { magnitude })
}

/// Compare the original decimal to an integer bound before rounding can hide
/// an out-of-range value. Answer domains have nonnegative integer endpoints.
pub(super) fn within_answer_range(raw: &str, upper: i128) -> bool {
    let negative = raw.starts_with('-');
    let raw = raw.trim_start_matches('-');
    let mut parts = raw.split(['e', 'E']);
    let mantissa = parts.next().unwrap_or("");
    let exponent: i32 = parts.next().and_then(|e| e.parse().ok()).unwrap_or(0);
    let whole_len = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let leading = digits.bytes().take_while(|b| *b == b'0').count();
    let digits = &digits[leading..];
    if digits.is_empty() {
        return true;
    }
    if negative || upper == 0 {
        return false;
    }
    let position = whole_len + exponent - leading as i32;
    let bound = upper.to_string();
    if position < bound.len() as i32 {
        return true;
    }
    if position > bound.len() as i32 {
        return false;
    }
    let integer: String = digits
        .chars()
        .take(position as usize)
        .chain(std::iter::repeat('0'))
        .take(position as usize)
        .collect();
    match integer.as_str().cmp(bound.as_str()) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => digits.bytes().skip(position as usize).all(|b| b == b'0'),
    }
}
