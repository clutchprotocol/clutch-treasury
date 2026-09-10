# Factual brief for legal counsel

Readiness item **J1**. What the system actually does, written so a lawyer can assess it without
reading the code and without billing for the discovery.

**This document contains no legal analysis and takes no position on the law.** It states facts and
lists the questions counsel needs to answer. Nobody on the engineering side is qualified to answer
them, and a wrong guess here is more expensive than the advice.

Prepared 2026-09-11. Everything described is the **testnet** as deployed today; nothing below has
ever held money of value.

## In one paragraph

Clutch Protocol is an open-source ride-sharing payment network. A rider pays a driver in CLT, a token
the operator issues against US-dollar-denominated stablecoin (USDT) it receives and holds. A user
sends USDT to an address the operator controls; the operator credits them CLT one-for-one at a fixed
rate; the user spends CLT on rides; the user may later destroy CLT and receive USDT back, less a fee.
The operator holds the USDT throughout.

## The facts, in the order they are likely to matter

**1. The operator holds user funds.** USDT sent by users is swept to a single custody address the
operator controls. This is the central fact: the token is not a claim on a third party, it is a claim
on the operator's own holdings.

**2. CLT is redeemable at a fixed rate.** 1 USD = 1,000,000 CLT, not floating, not market-determined.
Every unit in circulation is backed one-for-one by USDT in reserve. The operator publishes reserve
and liability figures, and the system halts issuance automatically if reserve stops covering
liability.

**3. Only the operator can create CLT.** Issuance requires a key the operator holds, and each
issuance is tied to a specific received payment. Destruction is permissionless — any holder may
destroy their own CLT — but receiving USDT back requires the operator to send it.

**4. CLT is used to pay for services.** Riders pay drivers. The network takes no share of a fare:
the driver receives the remainder after configurable referral fees that go to whoever operates the
application the ride came through, and validators receive a flat per-transaction fee. The operator's
only revenue is a fee charged when CLT is redeemed for USDT.

**5. There is no identity verification.** No KYC, no AML screening, no sanctions checking. An account
is a cryptographic keypair the user generates; the operator never learns who they are. Deposits are
attributed by which address received them, not by any identity claim.

**6. There is no geographic restriction.** The application is a public website. No jurisdiction is
excluded, no residency is asserted, no IP filtering exists.

**7. Users hold their own keys.** The operator cannot move a user's CLT, cannot freeze it, and cannot
reverse a payment. Losing the key loses the CLT irrecoverably, and the operator has no ability to
restore it.

**8. Payments are irreversible and there is no dispute mechanism.** A rider signs the payment
directly, so there is no card issuer and no chargeback. If a rider and driver disagree, nothing in
the system arbitrates. This is documented to users before they pay, and it is a known gap the
operator intends to close.

**9. Amounts are currently small and the software is alpha.** A single redemption is capped at $25;
issuance is capped at $50 per transaction and $500 per day. These are configurable.

**10. It is a payment network, not a marketplace operator.** The software is open source and anyone
may run their own application against the same chain, earning referral fees. The operator does not
employ drivers, set fares, or match riders to drivers beyond what the protocol does mechanically.

## Where things are

- **The reserve** is USDT held on the TRON network at an address the operator controls.
- **The issuing key** is currently an environment variable on a single virtual private server; moving
  it to a hardware-backed service is planned and is a prerequisite the operator has set for itself
  before real funds.
- **The operator** is one individual. There is currently no company, no second signatory, and no
  segregation between operating funds and user funds beyond the reserve address itself.
- **Users** are unidentified and unrestricted.

That third point is likely to matter as much as the technical ones.

## The questions

Grouped, and phrased as the operator's actual decisions rather than as legal theory.

**Characterisation.** What is CLT, in the operating jurisdiction? A stored-value instrument,
electronic money, a stablecoin under a specific regime, a deposit, or something else? Does the answer
change because it is only spendable within one network, or because it is redeemable at a fixed rate?

**Licensing.** Does issuing it, holding the reserve, or redeeming it require registration or a
licence, and in which jurisdictions — where the operator is, where users are, or both? What is the
consequence of having operated a public testnet with valueless tokens beforehand, if any?

**Customer due diligence.** Is KYC required, at what threshold, and for which side — depositors,
redeemers, drivers receiving payment, or all three? If it is required, note that the system currently
cannot identify anyone, so this is an architectural change rather than a policy one.

**Safeguarding.** Must user funds be segregated, held with a regulated institution, or covered by a
guarantee? Reserve is currently held as USDT at a self-custodied blockchain address, which may not
satisfy a safeguarding requirement however honest the accounting is.

**Consumer protection.** Is irreversibility with no dispute mechanism permissible for consumer
payments? Is disclosure sufficient, or is a remedy mandatory?

**Structure.** Should this operate through a company rather than an individual, and does that change
any of the above? Does the operator's own residence determine the regime, or the users'?

**Restriction as a mitigation.** If licensing is required somewhere but not everywhere, can the
operator lawfully exclude jurisdictions, and what does "excluding" have to mean in practice for a
public website and a permissionless chain?

**Tax and reporting.** Are issuance, redemption or the referral fee reportable events, and for whom?

## What the operator would like to know most

Whether there is a lawful configuration at small scale — low caps, restricted jurisdictions, clear
disclosures — that permits a limited real-funds pilot, and what that configuration would have to be.
If the answer is that any real-funds operation requires a licence first, that is a useful answer and
the operator would rather have it before building further.

## Supporting material, if useful

Public documentation at `docs.clutchprotocol.io`, including a page describing the reserve model and
one listing what is not yet built. Source at `github.com/clutchprotocol`. The reserve accounting,
issuance controls and redemption mechanics are described in `docs/` in the treasury repository. None
of it requires reading code to follow.
