# Key ceremony

Readiness item **A3**. `keys.md` names a key ceremony and tested recovery as mainnet blockers
without saying what either involves. You cannot hold a ceremony you have not written down, so this
is that procedure. It is a prerequisite for A3, not A3 itself: the item closes when the ceremony has
been *performed* and the record exists.

## What is different about a KMS key, and why it changes the ceremony

A traditional key ceremony exists because there is key material to generate, witness, split and
escrow. **With AWS KMS there is no key material to hold.** The key is generated inside the HSM and
cannot be exported — that is the entire reason for using it. So the usual centre of a ceremony,
splitting a seed among custodians, does not apply and should not be simulated.

What replaces it is narrower and easier to get wrong:

| Traditional ceremony | Here |
|---|---|
| Generate and witness key material | Witness that the key was created with the **right configuration**, since a wrong `KeySpec` produces signatures the node cannot verify |
| Split and escrow the seed | Nothing to split. Instead: record the key's **identity** (ARN, and the address derived from its public key) |
| Test that the seed reconstructs | Test that **access** recovers — because losing the account or the IAM path loses the key just as completely as losing a seed |

The third row is the one people skip. A KMS key with no tested access-recovery path is exactly as
lost-able as a seed phrase in one person's drawer; the failure just arrives as an IAM misconfiguration
rather than a house fire.

## Which keys

`keys.md` requires three, none interchangeable:

| Key | State today | Ceremony needed |
|---|---|---|
| **Mint** | Environment variable (`EnvKeySigner`) | Yes — this one first. It is the only key that can create CLT. |
| **Payout initiation** | Derived from the deposit mnemonic at `m/44'/195'/0'/2/0`, held by `tron-signer` | Yes, after the mint key. Bounded by the float balance and a per-transaction cap, which is why it is second rather than first. |
| **Reserve custody** | Does not exist in this stack, deliberately — nothing here can spend `APP_TREASURY_ADDRESS` | No KMS ceremony. It is a wallet a human holds, and what it needs is the two-person top-up procedure in A4, not this. |

Do the mint key alone, end to end, including the recovery test, before starting the payout key.
Two ceremonies on one afternoon is how a step gets skipped on the second.

## Before the day

- [ ] An AWS account that is **not** the one running anything else, so a compromise of the
      application's credentials is not a compromise of the signer.
- [ ] At least **two people present**, and neither of them alone able to complete the ceremony.
      One drives, one witnesses and records. If you cannot find a second person, stop: readiness
      item G3 is the same problem and it is not solved by proceeding.
- [ ] CloudTrail on, in that account, with its log destination outside it. The ceremony's own
      audit trail should not be deletable by the credentials used during the ceremony.
- [ ] This document read by both people beforehand, not during.

## The ceremony

Record every value marked **[record]** as you go, in the register described below. Do not
reconstruct it afterwards from memory or from the console.

1. **Create the key.**
   `KeySpec = ECC_SECG_P256K1`, `KeyUsage = SIGN_VERIFY`, origin `AWS_KMS`.
   **[record]** the key ARN and the creation timestamp.
   The witness reads the configuration back from the console independently — not from the driver's
   screen — and confirms both fields. A key created as `ECC_NIST_P256` will sign happily and produce
   signatures the node cannot verify, and the failure surfaces as a rejected mint, not as an error
   at creation.

2. **Disable deletion.** The key policy must **not** grant `kms:ScheduleKeyDeletion` to any
   principal that the application uses, and preferably to nobody. A key scheduled for deletion is a
   treasury that stops being able to mint after a waiting period nobody was watching.
   **[record]** which principals hold which `kms:*` actions.

3. **Derive and record the identity.**
   `GetPublicKey` → DER SPKI → the 65-byte uncompressed point →
   `clutch_chain::external_signature::address_from_uncompressed`.
   **[record]** the public key and the derived 0x address.
   The witness derives the address independently from the same public key and confirms it matches.
   This address becomes the mainnet `mint_authority` in the genesis parameters (C1), so an error here
   is baked into the genesis hash and cannot be corrected without a new chain.

4. **Sign a known value and verify it end to end.**
   Sign the digest for a throwaway transaction hash through the same code path production will use,
   and confirm the signature recovers to the address from step 3.
   `external_signature.rs` already tests this logic against the in-process signer, so what this step
   adds is proof that *this key, through this account,* behaves the same way.
   **[record]** the hash used, the resulting `(r, s, v)`, and that recovery matched.

5. **Test recovery — the step that is actually A3.**
   Losing access is losing the key. Establish and then *exercise* the path back:
   - a second principal, in a separate identity, that can sign with this key
   - and a written break-glass procedure for regaining administrative access to the account itself
   Then **use** the second principal to sign, from a machine that has never held the first one's
   credentials. **[record]** that it worked, and the date.
   A recovery path that has been designed but not exercised is not a recovery path. This is the
   distinction `keys.md` means by "tested recovery", and it is the reason this ceremony is not just
   step 1.

6. **Retire the predecessor.** Only after steps 1–5 are recorded: remove `APP_MINT_AUTHORITY_SECRET`
   from the host `.env`, confirm the service refuses to start in a configuration that would select
   `EnvKeySigner`, and prune the `.env.bak` that still contains it.
   **[record]** that the old secret is gone from the host. This is also what finally closes readiness
   item D2, since the mnemonic and the mint secret leave `.env` together.

## The register

One document, stored outside the AWS account, holding for each key: the ARN, the public key, the
derived address, the date, who was present, the policy summary, the test signature, and the date
recovery was last exercised.

It contains no secrets — by construction, since there are none to hold — so it can live wherever
your other operational records live. Its value is that six months later, when somebody asks whether
the address in the genesis parameters is really the KMS key's, there is an answer that does not
depend on anyone's memory.

Re-exercise recovery on a schedule and add a line each time. An untested recovery path decays
silently: IAM changes, people leave, and nothing tells you.

## If something goes wrong mid-ceremony

Stop and start over with a new key. Do not repair a partially completed ceremony. Keys are cheap,
the register is the artefact, and a key whose provenance is "we think we redid step 3" is worse than
no ceremony at all — it is a false record.

Nothing in this procedure is irreversible until step 6, which is deliberately last.
