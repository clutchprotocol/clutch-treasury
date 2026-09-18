# Key ceremony

Readiness item **A3**. `keys.md` names a key ceremony and tested recovery as mainnet blockers
without saying what either involves. You cannot hold a ceremony you have not written down, so this
is that procedure. It is a prerequisite for A3, not A3 itself: the item closes when the ceremony has
been *performed* and the record exists.

## What is different about a KMS key, and why it changes the ceremony

A traditional key ceremony exists because there is key material to generate, witness, split and
escrow. **With a cloud KMS key there is no key material to hold.** The key is generated inside the
vault and cannot be exported — that is the entire reason for using it. So the usual centre of a
ceremony, splitting a seed among custodians, does not apply and should not be simulated.

What replaces it is narrower and easier to get wrong:

| Traditional ceremony | Here |
|---|---|
| Generate and witness key material | Witness that the key was created with the **right configuration**, since a wrong key type or curve produces signatures the node cannot verify |
| Split and escrow the seed | Nothing to split. Instead: record the key's **identity** (its key identifier, and the address derived from its public key) |
| Test that the seed reconstructs | Test that **access** recovers — because losing the account or the permission path loses the key just as completely as losing a seed |

The third row is the one people skip. A KMS key with no tested access-recovery path is exactly as
lost-able as a seed phrase in one person's drawer; the failure just arrives as an IAM misconfiguration
rather than a house fire.

## Which keys

`keys.md` requires three roles, none interchangeable. The mint role is now **three keys, not one**
— see the next section.

| Key | State today | Ceremony needed |
|---|---|---|
| **Mint**, x3 | Environment variable (`EnvKeySigner`), single | Yes — these first. Together they are the only thing that can create CLT. |
| **Payout initiation** | Derived from the deposit mnemonic at `m/44'/195'/0'/2/0`, held by `tron-signer` | Yes, after the mint keys. Bounded by the float balance and a per-transaction cap, which is why it is second rather than first. |
| **Reserve custody** | Does not exist in this stack, deliberately — nothing here can spend `APP_TREASURY_ADDRESS` | No KMS ceremony. It is a wallet a human holds, and what it needs is the two-person top-up procedure in A4, not this. |

Do the mint keys end to end, including the recovery test, before starting the payout key. Two
ceremonies on one afternoon is how a step gets skipped on the second.

## The mint role is a 2-of-3

Decided 2026-09-12. The chain has supported M-of-N minting since 2026-09-11, and the configuration
chosen is **three authorities, any two of which must sign**. `mint_cosigners` and `mint_threshold`
are committed into the genesis hash, so this is settled before the mainnet chain boots and cannot
be changed afterwards without a new chain.

What that buys, precisely: a single compromised key mints nothing. The attacker needs two, and the
whole point of the placement below is that no single breach yields two.

**Put the three keys in three different places.** Three keys in one cloud account is a 2-of-3 on
paper and a 1-of-1 in practice, because one compromised account holds all of them.

| Key | Where | Why there |
|---|---|---|
| **A**, the submitter | Cloud KMS, the account `treasury-service` can reach | This is the one that signs the transaction envelope and needs to be callable by the running service. |
| **B** | A *different* provider or a different account with separate credentials | An attacker who takes the service's cloud account still has one key, not two. |
| **C**, the cold spare | Offline: paper or a hardware device in a safe, never on a networked machine | Not for routine minting. It exists so that losing A or B loses availability rather than the chain, since any two of three can still sign. |

Routine minting is therefore A plus B. C is the recovery path.

**A note on what this is and is not.** If one person holds all three, this is multi-*place*
control: an attacker must breach two separate stores rather than read one file, which is a real
and worthwhile gain. It is not multi-*person* control, and a compromised operator still mints.
That is readiness item G3.

**G3 is open by choice.** The maintainer decided on 2026-09-18 to launch without a second operator,
so this ceremony is written for one person. The two-person steps below have been *replaced*, not
deleted — the section after next says with what, and what has no replacement. If a second human ever
holds one of these keys, give them **B** and restore the witness steps at the same time.

**Generate and test all three before the genesis.** Each key's address goes into the genesis
configuration, and a wrong or missing address there cannot be corrected later. Run the
`check_authorisation` path against a throwaway chain with the three real addresses before
committing the mainnet genesis.

## Doing this alone

A witness did two separate jobs: catching the driver's mistake, and making a false record require
two people to agree on it. **The second job has no replacement.** The register below becomes your own
account of what you did, and nothing outside it can corroborate that account. Write that in the
register rather than leaving a blank where a witness name would go — a reader six months from now
needs to know the record is single-sourced, and a blank does not tell them.

The first job mostly survives, by making the second look come from a *different route* rather than a
different person:

| Where a step says "the witness confirms" | Alone, do this |
|---|---|
| Step 1, the key configuration | Read `KeySpec` and `KeyUsage` back with the CLI or API, **not** from the console page you created the key on. That page can show you what you typed rather than what was stored. |
| Step 3, the derived address | Do not re-derive it by hand. Step 4 already checks it, and checks it harder: a signature that recovers to this address proves it through a different code path, which a second person re-reading the same hex string does not. |
| Step 5, recovery | Unchanged, and now the most important step in this document. It was always about a second *identity*, never a second person. |

**Read each register entry back against the account before you start the next key.** Not from memory,
and not from the notes you have just typed — from the account itself. It costs five minutes and it is
where a mistyped address is still cheap to find.

This does not change the rule above about finishing each key end to end, including its recovery test,
before starting the next. Deferring a recovery test is how the third one never happens, and being
alone makes that more likely, not less.

## Before the day

- [ ] A cloud subscription or account that is **not** the one running anything else, so a
      compromise of the application's credentials is not a compromise of the signer. (Azure: a
      separate subscription and resource group. AWS: a separate account.)
- [ ] **"Doing this alone" above, read before you start.** There is no second person by decision,
      not by accident, so the independent checks in steps 1 and 3 are done differently rather than
      skipped. Skipping them is not what "doing this alone" means.
- [ ] Audit logging on for the vault, with its log destination outside it — Azure: a diagnostic
      setting sending to a Log Analytics workspace or storage account in a *different* resource
      group; AWS: CloudTrail. The ceremony's own audit trail should not be deletable by the
      credentials used during the ceremony.
- [ ] This document read in full beforehand, not during.

## The ceremony

Record every value marked **[record]** as you go, in the register described below. Do not
reconstruct it afterwards from memory or from the console.

**Run steps 1 to 4 three times, once per mint key, in the three locations named above.** Finish
each key completely, including the recovery test, before starting the next. Batching the three
creations and then doing three recovery tests at the end is how the third recovery test does not
happen. Record which of A, B or C each register entry is for, because an address on its own does
not say where its key lives, and that placement is the entire security property.

1. **Create the key — generated inside the vault, never imported.** Importing means the private
   key existed somewhere else at some point, which is exactly what this exists to avoid.
   - **Azure Key Vault:** Key type **EC**, curve **P-256K (SECP256K1)**, allowed operations
     **Sign** and **Verify**.
   - **AWS KMS:** `KeySpec = ECC_SECG_P256K1`, `KeyUsage = SIGN_VERIFY`, origin `AWS_KMS`.

   **[record]** the key identifier and the creation timestamp — Azure: the key's vault URL, name
   and version (e.g. `https://<vault>.vault.azure.net/keys/<name>/<version>`); AWS: the ARN.
   Read the curve and key type back from the portal or API **after** creation, not from memory of
   what you selected — the selection screen can show you what you clicked rather than what was
   actually stored. A key created with the wrong curve (Azure: plain **P-256**, not **P-256K**; AWS:
   `ECC_NIST_P256`) will sign happily and produce signatures the node cannot verify, and the failure
   surfaces as a rejected mint, not as an error at creation.

2. **Confirm nothing that signs can also delete.** The application's own principal must hold a role
   or policy that can sign and read the public key, and **nothing more**.
   - **Azure Key Vault:** the signing principal's only role on this vault is **Key Vault Crypto
     User** — that role has no delete permission. Not Crypto Officer, Administrator, Contributor, or
     Owner. Purge protection (turned on when the vault was created) is what makes this hold even
     against an account that *could* delete the vault itself: a deleted key still waits out the
     retention period before it is gone for good, which is time to notice and stop it.
   - **AWS KMS:** the key policy must not grant `kms:ScheduleKeyDeletion` to any principal the
     application uses, and preferably to nobody.

   **[record]** the exact role or policy assignment you checked, and that it excludes deletion.

3. **Derive and record the identity.**
   Fetch the public key and turn it into the 65-byte uncompressed point, then
   `clutch_chain::external_signature::address_from_uncompressed`.
   - **Azure Key Vault:** GetKey returns a JWK with separate `x` and `y` fields — the point is
     `0x04` followed by `x` then `y`, no unwrapping needed. See `azure_kms_signer.rs`'s
     `fetch_public_key` for the exact steps this project's own code takes.
   - **AWS KMS:** `GetPublicKey` returns a DER SPKI blob that has to be unwrapped to find the point.

   **[record]** the public key and the derived 0x address.
   Do not re-derive this by hand as a check — step 4 does it properly, through a different code
   path. Do not start step 4 for a different key before finishing it for this one.
   This address becomes the mainnet `mint_authority` in the genesis parameters (C1), so an error here
   is baked into the genesis hash and cannot be corrected without a new chain.

4. **Sign a known value and verify it end to end.**
   Sign the digest for a throwaway transaction hash through the same code path production will use
   — `AzureKmsSigner` (`crates/clutch-chain/src/azure_kms_signer.rs`) for Azure, a `KmsSigner`
   built to the `KMS_SIGNER_SHAPE` in `external_signature.rs` for AWS — and confirm the signature
   recovers to the address from step 3.
   `external_signature.rs` already tests this logic against the in-process signer, so what this step
   adds is proof that *this key, through this account,* behaves the same way.
   **[record]** the hash used, the resulting `(r, s, v)`, and that recovery matched.

5. **Test recovery — the step that is actually A3.**
   Losing access is losing the key. Establish and then *exercise* the path back:
   - a second principal, in a separate identity, that can sign with this key — Azure: a second App
     Registration, its own client ID and secret, granted the same Key Vault Crypto User role on
     this vault
   - and a written break-glass procedure for regaining administrative access to the subscription
     or account itself
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

One document, stored outside the cloud account or subscription the key lives in, holding for each
key: the key identifier (Azure: vault URL, name and version; AWS: the ARN), the public key, the
derived address, the date, who was present, the role or policy summary, the test signature, and the
date recovery was last exercised.

**"Who was present" is not optional when the answer is "nobody".** Write the operator's name and
`no witness — sole operator, G3 open`. A record with one name in it and no explanation reads, later,
like a record where somebody forgot to write the second name down.

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
