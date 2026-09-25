//! The treasury's half of the GasFree rail (docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md).
//!
//! A GasFree sweep pays the relay out of the USDT it moves, so a deposit to a GasFree account can
//! mint only what will still be there after that fee (spec §2). The treasury decides the fee from
//! its own settings and its own records, and caps what the orchestrator proposed: the
//! orchestrator is public-facing, and a fee it chose could be zero.

/// What a verified deposit to a GasFree account may mint.
#[derive(Debug, PartialEq)]
pub enum DepositMint {
    /// Mint the intent's amount, lowered to `cap` if it proposed more.
    Mint { cap: i64 },
    /// Below the minimum after the fee: mint nothing, and hold the deposit for a human. `cap` is
    /// still the most that may ever be minted for it.
    BelowMinimum { cap: i64 },
    /// The fee takes all of it: nothing can ever be minted for this deposit.
    NothingToMint,
}

/// `observed − fee` is the most a deposit may mint, and it is judged against the minimum (spec §2).
///
/// `fee` is `gasfree::fee_to_hold` for what the treasury has recorded about the account — never the
/// relay's `active` field, and never the chain's contract record alone (`record_account` in
/// tron_verifier.rs says why).
pub fn deposit_mint(observed_usdt: i64, fee_usdt: i64, min_deposit_usdt: i64) -> DepositMint {
    let cap = observed_usdt.saturating_sub(fee_usdt);
    if cap < 1 {
        DepositMint::NothingToMint
    } else if cap < min_deposit_usdt {
        DepositMint::BelowMinimum { cap }
    } else {
        DepositMint::Mint { cap }
    }
}

/// The relay's record of a permit, as the signer passes it on (`GET /internal/gasfree/trace/:id`).
/// Only ever a cross-check, or a pointer to a transaction: the chain decides what happened.
#[derive(Debug, Clone, PartialEq)]
pub struct Trace {
    /// `WAITING`, `INPROGRESS`, `CONFIRMING`, `SUCCEED` or `FAILED`.
    pub state: String,
    pub txn_hash: Option<String>,
    /// What reached the receiver.
    pub txn_amount: Option<i64>,
}

/// The signer's trace reply, read field by field.
pub fn parse_trace(body: &serde_json::Value) -> Result<Trace, String> {
    todo!("Task 3 Step 5")
}

/// `GET /internal/gasfree/trace/:trace_id` on the signer. The signer holds the relay's API key;
/// this service does not.
pub async fn fetch_trace(http: &reqwest::Client, base_url: &str, token: &str, trace_id: &str) -> Result<Trace, String> {
    todo!("Task 3 Step 5")
}

/// Why GasFree must stop, when its code is not the reviewed code; `None` when it is (spec §5). Both
/// proxies: the beacon behind every GasFree account, and the controller that moves money out of
/// them, which is upgradeable too.
pub async fn code_changed(
    client: &crate::tron_verifier::TronClient,
    settings: &gasfree::Settings,
) -> Result<Option<String>, String> {
    todo!("Task 3 Step 5")
}

#[cfg(test)]
mod tests {
    use super::{deposit_mint, DepositMint};

    // The maxima the design sizes against Nile's live fees, 1.00 and 0.30 (spec §2).
    const ACTIVATE_MAX: i64 = 1_500_000;
    const TRANSFER_MAX: i64 = 500_000;
    const MIN_DEPOSIT: i64 = 1_000_000;
    const TEN: i64 = 10_000_000;

    #[test]
    fn the_cap_is_what_arrived_less_the_fee() {
        assert_eq!(deposit_mint(TEN, 2_000_000, MIN_DEPOSIT), DepositMint::Mint { cap: 8_000_000 });
    }

    #[test]
    fn below_the_minimum_after_the_fee_is_held_not_minted() {
        assert_eq!(deposit_mint(2_500_000, 2_000_000, MIN_DEPOSIT), DepositMint::BelowMinimum { cap: 500_000 });
        assert_eq!(deposit_mint(3_000_000, 2_000_000, MIN_DEPOSIT), DepositMint::Mint { cap: 1_000_000 }, "exactly the minimum mints");
    }

    #[test]
    fn a_deposit_the_fee_takes_whole_mints_nothing_ever() {
        assert_eq!(deposit_mint(2_000_000, 2_000_000, MIN_DEPOSIT), DepositMint::NothingToMint);
        assert_eq!(deposit_mint(1, 2_000_000, MIN_DEPOSIT), DepositMint::NothingToMint);
    }

    /// Spec §8, "the reserve rule, directly": one GasFree account, custody and the float as the
    /// reserve counts them, and the CLT in circulation. The treasury's side runs the real functions;
    /// the signer and the relay are modelled on what the signer code and a permit allow.
    struct World {
        /// The GasFree account's balance. Counted in the reserve for as long as it holds anything.
        g: i64,
        custody: i64,
        float: i64,
        supply: i64,
        /// What the chain says, which is what the signer sizes `maxFee` from.
        activated_on_chain: bool,
        /// What the treasury recorded, which is what it sizes its hold from.
        treasury_saw_first_transfer: bool,
        activate_live: i64,
        transfer_live: i64,
    }

    impl World {
        /// Nile's live fees on 2026-09-24.
        fn nile() -> Self {
            World {
                g: 0,
                custody: 0,
                float: 0,
                supply: 0,
                activated_on_chain: false,
                treasury_saw_first_transfer: false,
                activate_live: 1_000_000,
                transfer_live: 300_000,
            }
        }

        fn surplus(&self) -> i64 {
            self.g + self.custody + self.float - self.supply
        }

        fn backed(&self, step: &str) {
            assert!(self.surplus() >= 0, "{step}: the reserve is {} below the supply", -self.surplus());
        }

        /// The treasury verifies a deposit of `observed`, the orchestrator having proposed all of it.
        fn verify(&mut self, observed: i64) -> DepositMint {
            let fee = gasfree::fee_to_hold(self.treasury_saw_first_transfer, ACTIVATE_MAX, TRANSFER_MAX);
            let decision = deposit_mint(observed, fee, MIN_DEPOSIT);
            if let DepositMint::Mint { cap } = decision {
                self.supply += cap;
            }
            decision
        }

        /// The signer sweeps the account: `maxFee` from the chain, `value = balance − maxFee`, and
        /// the relay takes its live fee on top of `value`. `by_treasury`: the treasury's own sweeper
        /// asked, so it records the first transfer.
        fn sweep(&mut self, by_treasury: bool, to_float: bool) -> bool {
            let max_fee = gasfree::fee_to_hold(self.activated_on_chain, ACTIVATE_MAX, TRANSFER_MAX);
            let live = if self.activated_on_chain { self.transfer_live } else { self.activate_live + self.transfer_live };
            // The signer's below_fee, and the relay refusing a permit whose maxFee is under its fee.
            if self.g <= max_fee || live > max_fee {
                return false;
            }
            let value = self.g - max_fee;
            if to_float {
                self.float += value;
            } else {
                self.custody += value;
            }
            self.g -= value + live;
            self.activated_on_chain = true;
            if by_treasury {
                self.treasury_saw_first_transfer = true;
            }
            true
        }
    }

    #[test]
    fn the_reserve_covers_supply_through_a_first_and_a_later_deposit() {
        let mut w = World::nile();
        w.g += TEN;
        assert_eq!(w.verify(TEN), DepositMint::Mint { cap: 8_000_000 }, "a first deposit holds activation and one transfer");
        w.backed("first deposit verified");
        assert!(w.sweep(true, false));
        w.backed("first deposit swept");
        assert_eq!(w.g, 700_000, "the relay's unused margin stays in the account, still counted");

        w.g += TEN;
        assert_eq!(w.verify(TEN), DepositMint::Mint { cap: 9_500_000 }, "after its own first sweep ran, one transfer fee");
        w.backed("later deposit verified");
        assert!(w.sweep(true, false));
        w.backed("later deposit swept");
    }

    #[test]
    fn two_deposits_swept_together_over_reserve_by_one_fee() {
        let mut w = World::nile();
        w.g += TEN;
        w.verify(TEN);
        w.g += TEN;
        w.verify(TEN);
        assert!(w.sweep(true, false));
        w.backed("two deposits, one sweep");
        assert_eq!(w.surplus(), 2_000_000 + 700_000, "one extra hold, plus the sweep's unused margin");
    }

    /// Plan 2's final review (I2): a sweep that runs before the deposit it moves is credited
    /// activates the account first. A treasury that trusted the chain's record would then hold one
    /// transfer fee for a deposit that paid activation too.
    #[test]
    fn a_sweep_before_the_credit_is_covered_because_the_hold_follows_the_treasurys_own_record() {
        let mut w = World::nile();
        w.g += TEN;
        assert!(w.sweep(false, false), "someone reached the signer before the credit");
        assert_eq!(w.verify(TEN), DepositMint::Mint { cap: 8_000_000 }, "the treasury did not see that sweep, so it holds both fees");
        w.backed("early sweep, then the credit");

        let from_the_chain = deposit_mint(TEN, gasfree::fee_to_hold(true, ACTIVATE_MAX, TRANSFER_MAX), MIN_DEPOSIT);
        assert_eq!(from_the_chain, DepositMint::Mint { cap: 9_500_000 });
        assert!(w.g + w.custody < 9_500_000, "sized from the chain, the same deposit would be under-reserved");
    }

    #[test]
    fn a_live_fee_above_the_maximum_moves_nothing_and_leaves_the_deposit_counted() {
        let mut w = World::nile();
        w.activate_live = 2_000_000;
        w.g += TEN;
        w.verify(TEN);
        assert!(!w.sweep(true, false), "the relay refuses a permit whose maxFee is below its fee");
        w.backed("refused sweep");
        assert_eq!(w.g, TEN, "the deposit stays where it is, still counted");
    }

    #[test]
    fn below_the_minimum_nothing_is_minted_and_the_deposit_still_counts() {
        let mut w = World::nile();
        w.g += 2_500_000;
        assert_eq!(w.verify(2_500_000), DepositMint::BelowMinimum { cap: 500_000 });
        assert_eq!(w.supply, 0);
        w.backed("below the minimum");
    }

    /// Spec §4 and invariant 1: a redemption keeps back REDEMPTION_FEE_USDT, and the relay's fee for
    /// the payout comes out of the (activated) float on top of what the redeemer gets.
    #[test]
    fn a_redemption_paid_from_the_gasfree_float_keeps_the_reserve_whole() {
        let mut w = World::nile();
        w.g += TEN;
        w.verify(TEN);
        assert!(w.sweep(true, true), "below its target, the float takes the sweep");
        w.backed("float filled");
        let redemption_fee = TRANSFER_MAX; // the smallest the invariant allows
        let burned = 5_000_000;
        w.supply -= burned;
        w.float -= (burned - redemption_fee) + w.transfer_live;
        w.backed("redemption paid");
    }

    /// Spec §4: the float's activation is paid from surplus, and the workflow that asks for it
    /// refuses unless the reserve leads supply by activation plus one transfer, at their maxima.
    #[test]
    fn the_floats_activation_is_covered_by_the_surplus_the_workflow_requires() {
        let mut w = World::nile();
        w.g += TEN;
        w.verify(TEN);
        assert!(w.sweep(true, true));
        let gate = ACTIVATE_MAX + TRANSFER_MAX;
        assert!(w.surplus() < gate, "one first deposit's margin is not enough, so the workflow refuses");
        w.custody += gate - w.surplus(); // later deposits' margins, or a top-up
        // One micro-USDT from the float to custody, and the relay's live fees for a first transfer.
        w.float -= 1 + w.activate_live + w.transfer_live;
        w.custody += 1;
        w.backed("float activated from surplus");
    }
}
