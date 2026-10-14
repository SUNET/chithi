//! A narrow equivalence proof for duplicate provider rules. Different cutoffs
//! are harmless only when the unchanged pattern has no starts between them.

use std::collections::BTreeMap;

use super::*;

pub(crate) fn equivalent_rule_cutoffs(rules: &[&str], event: &CalendarEvent) -> Result<String> {
    if rules.is_empty() || rules.len() > 128 {
        return Err(invalid("RRULE equivalence requires 1..=128 rules"));
    }
    let mut selectors = None;
    let mut earliest = None;
    let mut latest = None;
    let mut bounded = None;
    let mut normalized = String::new();
    for rule in rules {
        let text = normalize_rule(rule, event)?;
        let mut parts = text
            .split(';')
            .map(|part| {
                part.split_once('=')
                    .ok_or_else(|| invalid("malformed rule in equivalence proof"))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let cutoff = parts
            .remove("UNTIL")
            .map(|value| {
                NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ")
                    .map(|value| value.and_utc())
                    .map_err(invalid)
            })
            .transpose()?;
        if bounded.is_some_and(|previous| previous != cutoff.is_some()) {
            return Err(invalid("cannot equate bounded and unbounded RRULEs"));
        }
        bounded = Some(cutoff.is_some());
        let parts = parts
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect::<BTreeMap<_, _>>();
        if selectors
            .as_ref()
            .is_some_and(|previous| previous != &parts)
        {
            return Err(invalid("RRULE selectors differ"));
        }
        if normalized.is_empty() {
            normalized = text.clone();
        }
        selectors = Some(parts);
        if let Some(cutoff) = cutoff {
            earliest =
                Some(earliest.map_or(cutoff, |previous: DateTime<Utc>| previous.min(cutoff)));
            latest = Some(latest.map_or(cutoff, |previous: DateTime<Utc>| previous.max(cutoff)));
        }
    }
    let (Some(earliest), Some(latest)) = (earliest, latest) else {
        return Ok(normalized);
    };
    // The rules agree through the earlier inclusive cutoff. Prove the complete
    // difference, rather than sampling a visible month or comparing event ends.
    if earliest != latest {
        let upper = latest
            .checked_add_signed(Duration::nanoseconds(1))
            .ok_or_else(|| invalid("RRULE equivalence cutoff overflow"))?;
        let mut candidate = event.clone();
        candidate.recurrence_rule = Some(
            normalized
                .split(';')
                .filter(|part| !part.starts_with("UNTIL="))
                .collect::<Vec<_>>()
                .join(";"),
        );
        let mut differs = false;
        visit_original_starts(&candidate, earliest, upper, |start| {
            if start > earliest {
                differs = true;
                return Ok(ControlFlow::Break(()));
            }
            Ok(ControlFlow::Continue(()))
        })?;
        if differs {
            return Err(invalid(
                "RRULE cutoffs include different original positions",
            ));
        }
    }
    Ok(normalized
        .split(';')
        .map(|part| {
            if part.starts_with("UNTIL=") {
                format!("UNTIL={}", earliest.format("%Y%m%dT%H%M%SZ"))
            } else {
                part.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(";"))
}
