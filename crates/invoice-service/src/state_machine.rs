//! Invoice state machine: the single source of truth for which transitions are
//! legal. The SQL layer derives its `WHERE status = ANY(...)` guards from here,
//! so the code and the database agree on what is allowed.
//!
//! ```text
//!  (create) ──▶ draft ──finalize──▶ open ──pay──▶ processing ──psp succeeded──▶ paid ■
//!                 │                  │  ▲             │
//!                 │                  │  └─psp failed──┤ (back to where it came from)
//!                 └──void──▶ void ■ ◀┘void            │
//!                                    │                ▼
//!                                    └─mark_uncollectible──▶ uncollectible ──pay──▶ processing
//! ■ = terminal (paid, void). uncollectible is NOT terminal: a late payment is still accepted.
//! ```

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceStatus {
    Draft,
    Open,
    Processing,
    Paid,
    Void,
    Uncollectible,
}

pub const ALL_STATUSES: [InvoiceStatus; 6] = [
    InvoiceStatus::Draft,
    InvoiceStatus::Open,
    InvoiceStatus::Processing,
    InvoiceStatus::Paid,
    InvoiceStatus::Void,
    InvoiceStatus::Uncollectible,
];

impl InvoiceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            InvoiceStatus::Draft => "draft",
            InvoiceStatus::Open => "open",
            InvoiceStatus::Processing => "processing",
            InvoiceStatus::Paid => "paid",
            InvoiceStatus::Void => "void",
            InvoiceStatus::Uncollectible => "uncollectible",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        ALL_STATUSES.into_iter().find(|status| status.as_str() == s)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, InvoiceStatus::Paid | InvoiceStatus::Void)
    }
}

impl fmt::Display for InvoiceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Things that can happen to an invoice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// API: POST /invoices/{id}/finalize
    Finalize,
    /// API: POST /invoices/{id}/void
    Void,
    /// API: POST /invoices/{id}/mark_uncollectible
    MarkUncollectible,
    /// API: POST /invoices/{id}/pay (a payment attempt starts)
    Pay,
    /// PSP confirmed the charge (request handler or reconciler).
    PaymentSucceeded,
    /// PSP declined, or confirmed it never charged. Returns the invoice to the
    /// status it had before the payment started.
    PaymentFailed { previous: InvoiceStatus },
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::Finalize => "finalize",
            Action::Void => "void",
            Action::MarkUncollectible => "mark_uncollectible",
            Action::Pay => "pay",
            Action::PaymentSucceeded => "payment_succeeded",
            Action::PaymentFailed { .. } => "payment_failed",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct InvalidTransition {
    pub from: InvoiceStatus,
    pub action: Action,
}

pub fn transition(from: InvoiceStatus, action: Action) -> Result<InvoiceStatus, InvalidTransition> {
    use InvoiceStatus::*;
    let to = match (from, action) {
        (Draft, Action::Finalize) => Open,
        (Draft | Open, Action::Void) => Void,
        (Open, Action::MarkUncollectible) => Uncollectible,
        (Open | Uncollectible, Action::Pay) => Processing,
        (Processing, Action::PaymentSucceeded) => Paid,
        (
            Processing,
            Action::PaymentFailed {
                previous: previous @ (Open | Uncollectible),
            },
        ) => previous,
        _ => return Err(InvalidTransition { from, action }),
    };
    Ok(to)
}

/// All statuses from which `action` is legal, for SQL guards
/// (`UPDATE ... WHERE status = ANY($n)`).
pub fn allowed_from(action: Action) -> Vec<&'static str> {
    ALL_STATUSES
        .into_iter()
        .filter(|from| transition(*from, action).is_ok())
        .map(InvoiceStatus::as_str)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Action as A, InvoiceStatus as S, *};

    #[test]
    fn full_transition_table() {
        let actions = [
            A::Finalize,
            A::Void,
            A::MarkUncollectible,
            A::Pay,
            A::PaymentSucceeded,
            A::PaymentFailed { previous: S::Open },
            A::PaymentFailed {
                previous: S::Uncollectible,
            },
        ];
        let expected: &[(S, A, S)] = &[
            (S::Draft, A::Finalize, S::Open),
            (S::Draft, A::Void, S::Void),
            (S::Open, A::Void, S::Void),
            (S::Open, A::MarkUncollectible, S::Uncollectible),
            (S::Open, A::Pay, S::Processing),
            (S::Uncollectible, A::Pay, S::Processing),
            (S::Processing, A::PaymentSucceeded, S::Paid),
            (
                S::Processing,
                A::PaymentFailed { previous: S::Open },
                S::Open,
            ),
            (
                S::Processing,
                A::PaymentFailed {
                    previous: S::Uncollectible,
                },
                S::Uncollectible,
            ),
        ];
        // Every (status, action) pair is either in `expected` or rejected.
        for from in ALL_STATUSES {
            for action in actions {
                let want = expected
                    .iter()
                    .find(|(f, a, _)| *f == from && *a == action)
                    .map(|(_, _, to)| *to);
                assert_eq!(
                    transition(from, action).ok(),
                    want,
                    "{from} --{}-->",
                    action.name()
                );
            }
        }
    }

    #[test]
    fn terminal_states_have_no_exits() {
        for from in [S::Paid, S::Void] {
            assert!(from.is_terminal());
            for action in [
                A::Finalize,
                A::Void,
                A::MarkUncollectible,
                A::Pay,
                A::PaymentSucceeded,
            ] {
                assert!(transition(from, action).is_err());
            }
        }
    }

    #[test]
    fn cannot_void_or_write_off_while_money_is_in_flight() {
        assert!(transition(S::Processing, A::Void).is_err());
        assert!(transition(S::Processing, A::MarkUncollectible).is_err());
        assert!(transition(S::Processing, A::Pay).is_err());
    }

    #[test]
    fn sql_guards_match_table() {
        assert_eq!(allowed_from(A::Pay), vec!["open", "uncollectible"]);
        assert_eq!(allowed_from(A::Void), vec!["draft", "open"]);
        assert_eq!(allowed_from(A::Finalize), vec!["draft"]);
    }
}
