use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingUnit {
    Free,
    Unknown,
    Character,
    Utf8Byte,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UnknownCostReason {
    ProviderUnitUnavailable,
    AdditionalRateUnitUnavailable,
    InvalidRate,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EstimateSource {
    Free,
    User,
}
pub struct CostEstimate {
    pub unit: PricingUnit,
    pub units: Option<usize>,
    pub amount: Option<String>,
    pub reason: Option<UnknownCostReason>,
    pub source: Option<EstimateSource>,
}
pub const fn unknown(reason: UnknownCostReason) -> CostEstimate {
    CostEstimate {
        unit: PricingUnit::Unknown,
        units: None,
        amount: None,
        reason: Some(reason),
        source: None,
    }
}
pub fn parse_decimal(value: &str) -> Option<(u128, i32)> {
    let value = value.trim();
    let (mantissa, exponent) = value
        .split_once(['e', 'E'])
        .map_or((value, "0"), |(a, b)| (a, b));
    let exponent: i32 = exponent.parse().ok()?;
    if !(-128..=128).contains(&exponent) {
        return None;
    }
    let (whole, fraction) = mantissa
        .split_once('.')
        .map_or((mantissa, ""), |(a, b)| (a, b));
    if whole.is_empty()
        || !whole.bytes().all(|c| c.is_ascii_digit())
        || !fraction.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let coefficient: u128 = digits
        .trim_start_matches('0')
        .parse()
        .or_else(|_| {
            if digits.bytes().all(|c| c == b'0') {
                Ok(0_u128)
            } else {
                Err(())
            }
        })
        .ok()?;
    Some((
        coefficient,
        exponent.checked_sub(i32::try_from(fraction.len()).ok()?)?,
    ))
}
pub fn multiply_decimal(rate: &str, units: usize) -> Option<String> {
    let (coefficient, exponent) = parse_decimal(rate)?;
    let product = coefficient.checked_mul(u128::try_from(units).ok()?)?;
    if product == 0 {
        return Some("0".into());
    }
    let mut digits = product.to_string();
    if exponent >= 0 {
        let count = usize::try_from(exponent).ok()?;
        if digits.len().checked_add(count)? > 128 {
            return None;
        }
        digits.push_str(&"0".repeat(count));
        return Some(digits);
    }
    let places = usize::try_from(exponent.checked_neg()?).ok()?;
    if places > 128 {
        return None;
    }
    if places >= digits.len() {
        digits = format!("0.{}{}", "0".repeat(places - digits.len()), digits);
    } else {
        digits.insert(digits.len() - places, '.');
    }
    while digits.ends_with('0') {
        digits.pop();
    }
    if digits.ends_with('.') {
        digits.pop();
    }
    Some(digits)
}
fn aligned_decimals(values: &[&str]) -> Option<(Vec<u128>, i32)> {
    let parsed = values
        .iter()
        .map(|value| parse_decimal(value))
        .collect::<Option<Vec<_>>>()?;
    let min_exp = parsed.iter().map(|(_, exponent)| *exponent).min()?;
    let numbers = parsed
        .into_iter()
        .map(|(value, exponent)| {
            let power = u32::try_from(exponent.checked_sub(min_exp)?).ok()?;
            value.checked_mul(10_u128.checked_pow(power)?)
        })
        .collect::<Option<Vec<_>>>()?;
    Some((numbers, min_exp))
}
pub fn decimal_add(left: &str, right: &str) -> Option<String> {
    let (values, exponent) = aligned_decimals(&[left, right])?;
    let sum = values[0].checked_add(values[1])?;
    multiply_decimal(&format!("{sum}e{exponent}"), 1)
}
pub fn decimal_add_compare(left: &str, right: &str, limit: &str) -> Option<std::cmp::Ordering> {
    let (values, _) = aligned_decimals(&[left, right, limit])?;
    Some(values[0].checked_add(values[1])?.cmp(&values[2]))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_arithmetic_and_fail_closed_overflow() {
        assert_eq!(
            multiply_decimal("0.000015", 17).as_deref(),
            Some("0.000255")
        );
        assert_eq!(
            decimal_add("0.0000001", "0.00000002").as_deref(),
            Some("0.00000012")
        );
        assert_eq!(
            decimal_add_compare("0.1", "0.2", "0.3"),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!(
            decimal_add_compare("0.1", "0.20001", "0.3"),
            Some(std::cmp::Ordering::Greater)
        );
        assert!(multiply_decimal("1e128", 100).is_none());
        assert!(decimal_add("340282366920938463463374607431768211455", "1").is_none());
        assert!(decimal_add_compare("1e128", "1", "2").is_none());
        assert!(parse_decimal("-1").is_none());
        assert!(parse_decimal("not-a-rate").is_none());
    }
}
