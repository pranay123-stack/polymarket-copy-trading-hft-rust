//! Position reconciliation against the venue.
//!
//! A mismatch between what we think we hold and what the venue says we hold is **never
//! auto-corrected and never ignored**. It is surfaced as an alert, and the policy is
//! configurable up to engaging the kill switch, because trading on a wrong position is
//! how a small bug becomes a large loss.
//!
//! Reconciliation also resolves orders stuck in `UNKNOWN`: if the venue shows quantity we
//! do not have booked, the order did execute.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use domain::{Qty, TokenId};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::adapter::VenuePosition;

/// One position disagreement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mismatch {
    pub token_id: TokenId,
    /// What we believe, signed.
    pub internal: Decimal,
    /// What the venue reports, signed.
    pub venue: Decimal,
    pub difference: Decimal,
    pub at: DateTime<Utc>,
}

impl Mismatch {
    /// Relative size of the disagreement, for severity triage.
    pub fn relative(&self) -> Decimal {
        let d = self.internal.abs().max(self.venue.abs());
        if d.is_zero() { Decimal::ZERO } else { (self.difference.abs() / d).min(Decimal::ONE) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReconciliationReport {
    pub at: DateTime<Utc>,
    pub checked: usize,
    pub mismatches: Vec<Mismatch>,
    /// Tokens the venue reports that we have no record of at all — the most serious case.
    pub unexpected_venue_positions: Vec<TokenId>,
    /// Tokens we think we hold but the venue does not report.
    pub missing_at_venue: Vec<TokenId>,
}

impl ReconciliationReport {
    pub fn is_clean(&self) -> bool {
        self.mismatches.is_empty()
            && self.unexpected_venue_positions.is_empty()
            && self.missing_at_venue.is_empty()
    }

    /// Serious enough to stop trading?
    ///
    /// An unexpected venue position means we are exposed in a market we do not know
    /// about — always halt. A quantity disagreement halts once it exceeds `tolerance`.
    pub fn warrants_halt(&self, tolerance: Decimal) -> bool {
        !self.unexpected_venue_positions.is_empty()
            || self.mismatches.iter().any(|m| m.difference.abs() > tolerance)
    }

    pub fn summary(&self) -> String {
        if self.is_clean() {
            return format!("clean: {} positions agree", self.checked);
        }
        format!(
            "{} mismatches, {} unexpected at venue, {} missing at venue",
            self.mismatches.len(),
            self.unexpected_venue_positions.len(),
            self.missing_at_venue.len()
        )
    }
}

pub struct Reconciler;

impl Reconciler {
    /// Compares our position book against the venue's.
    ///
    /// `tolerance` absorbs benign rounding only; it is not a licence to ignore real
    /// disagreements, and a difference above it is always reported.
    pub fn reconcile(
        internal: &HashMap<TokenId, Decimal>,
        venue: &[VenuePosition],
        tolerance: Decimal,
        now: DateTime<Utc>,
    ) -> ReconciliationReport {
        let venue_map: HashMap<TokenId, Decimal> =
            venue.iter().map(|p| (p.token_id.clone(), p.quantity.get())).collect();

        let mut mismatches = Vec::new();
        let mut missing = Vec::new();

        for (token, ours) in internal {
            match venue_map.get(token) {
                Some(theirs) => {
                    let diff = *ours - *theirs;
                    if diff.abs() > tolerance {
                        mismatches.push(Mismatch {
                            token_id: token.clone(),
                            internal: *ours,
                            venue: *theirs,
                            difference: diff,
                            at: now,
                        });
                    }
                }
                None if ours.abs() > tolerance => missing.push(token.clone()),
                None => {}
            }
        }

        let known: HashSet<&TokenId> = internal.keys().collect();
        let unexpected: Vec<TokenId> = venue_map
            .iter()
            .filter(|(t, q)| !known.contains(*t) && q.abs() > tolerance)
            .map(|(t, _)| t.clone())
            .collect();

        ReconciliationReport {
            at: now,
            checked: internal.len().max(venue_map.len()),
            mismatches,
            unexpected_venue_positions: unexpected,
            missing_at_venue: missing,
        }
    }

    /// Does the venue show quantity implying an `UNKNOWN` order actually executed?
    pub fn resolves_unknown_order(
        token: &TokenId,
        booked: Decimal,
        venue: &[VenuePosition],
        order_qty: Qty,
        tolerance: Decimal,
    ) -> bool {
        let v = venue.iter().find(|p| &p.token_id == token).map(|p| p.quantity.get()).unwrap_or(Decimal::ZERO);
        (v - booked).abs() >= order_qty.get() - tolerance && order_qty.get() > Decimal::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn tok(n: u8) -> TokenId { TokenId::new(format!("{}", 1000 + n as u32)).unwrap() }
    fn vp(n: u8, q: Decimal) -> VenuePosition {
        VenuePosition { token_id: tok(n), quantity: Qty::new(q).unwrap() }
    }

    #[test]
    fn agreeing_books_reconcile_clean() {
        let internal = HashMap::from([(tok(1), dec!(100)), (tok(2), dec!(50))]);
        let r = Reconciler::reconcile(&internal, &[vp(1, dec!(100)), vp(2, dec!(50))], dec!(0.01), Utc::now());
        assert!(r.is_clean());
        assert!(!r.warrants_halt(dec!(1)));
        assert!(r.summary().starts_with("clean"));
    }

    #[test]
    fn quantity_disagreement_is_reported_not_absorbed() {
        let internal = HashMap::from([(tok(1), dec!(100))]);
        let r = Reconciler::reconcile(&internal, &[vp(1, dec!(80))], dec!(0.01), Utc::now());
        assert_eq!(r.mismatches.len(), 1);
        assert_eq!(r.mismatches[0].difference, dec!(20));
        assert!(!r.is_clean());
    }

    #[test]
    fn tolerance_absorbs_rounding_but_not_real_gaps() {
        let internal = HashMap::from([(tok(1), dec!(100.0001))]);
        let r = Reconciler::reconcile(&internal, &[vp(1, dec!(100))], dec!(0.01), Utc::now());
        assert!(r.is_clean(), "sub-tolerance rounding must not raise noise");

        let internal = HashMap::from([(tok(1), dec!(101))]);
        let r = Reconciler::reconcile(&internal, &[vp(1, dec!(100))], dec!(0.01), Utc::now());
        assert_eq!(r.mismatches.len(), 1, "a real gap must always surface");
    }

    #[test]
    fn unexpected_venue_position_always_warrants_a_halt() {
        // We are exposed in a market we do not know we are in.
        let internal = HashMap::new();
        let r = Reconciler::reconcile(&internal, &[vp(9, dec!(500))], dec!(0.01), Utc::now());
        assert_eq!(r.unexpected_venue_positions.len(), 1);
        // Even a generous tolerance must not suppress this.
        assert!(r.warrants_halt(dec!(1_000_000)));
    }

    #[test]
    fn position_missing_at_venue_is_flagged() {
        let internal = HashMap::from([(tok(1), dec!(100))]);
        let r = Reconciler::reconcile(&internal, &[], dec!(0.01), Utc::now());
        assert_eq!(r.missing_at_venue, vec![tok(1)]);
        assert!(!r.is_clean());
    }

    #[test]
    fn flat_internal_positions_do_not_generate_noise() {
        let internal = HashMap::from([(tok(1), Decimal::ZERO)]);
        let r = Reconciler::reconcile(&internal, &[], dec!(0.01), Utc::now());
        assert!(r.is_clean());
    }

    #[test]
    fn halt_threshold_scales_with_tolerance() {
        let internal = HashMap::from([(tok(1), dec!(100))]);
        let r = Reconciler::reconcile(&internal, &[vp(1, dec!(95))], dec!(0.01), Utc::now());
        assert!(r.warrants_halt(dec!(1)), "a 5-share gap should halt at tolerance 1");
        assert!(!r.warrants_halt(dec!(10)), "and be tolerated at tolerance 10");
    }

    #[test]
    fn relative_severity_is_bounded() {
        let m = Mismatch { token_id: tok(1), internal: dec!(100), venue: dec!(50),
            difference: dec!(50), at: Utc::now() };
        assert_eq!(m.relative(), dec!(0.5));
        let m = Mismatch { token_id: tok(1), internal: Decimal::ZERO, venue: Decimal::ZERO,
            difference: Decimal::ZERO, at: Utc::now() };
        assert_eq!(m.relative(), Decimal::ZERO, "must not divide by zero");
    }

    #[test]
    fn venue_quantity_can_resolve_an_unknown_order() {
        // We booked nothing, but the venue holds 100 -> the UNKNOWN order did execute.
        let venue = [vp(1, dec!(100))];
        assert!(Reconciler::resolves_unknown_order(
            &tok(1), Decimal::ZERO, &venue, Qty::new(dec!(100)).unwrap(), dec!(0.01)));
        // Venue agrees with our books -> the order did not execute.
        assert!(!Reconciler::resolves_unknown_order(
            &tok(1), dec!(100), &venue, Qty::new(dec!(100)).unwrap(), dec!(0.01)));
    }
}
