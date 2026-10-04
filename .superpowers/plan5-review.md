# Plan 5 review: mainnet treasury readiness

Plan: `clutch-treasury/docs/superpowers/plans/2026-10-02-mainnet-treasury-readiness.md`.
Checked against `clutch-deploy` at `150b0a5` (main). Read only. Nothing was run.

1. [Critical] plan line 925 (CI steps added at 246-251 and 896-901) — Task 2's red state cannot be seen in CI. In `test-treasury-scripts.yml` a step runs only if every step before it passed. In Task 2's red commit the second step, "Cap invariants", fails (3 new cases). So "GasFree fee check", "Float activation decision", "GasFree settings writer", "Mainnet compose guard" and "Chain helper" are skipped. The log never shows the 9 FAIL lines of `test-chain.sh`, so the controller cannot confirm the red state. — Fix: in Task 1 Step 2, add `if: ${{ !cancelled() }}` to every test step (Cap invariants, GasFree fee check, Float activation decision, GasFree settings writer, Mainnet compose guard). Put the same line in the new steps of Tasks 2, 3 and 4 (Chain helper, Chain-aware tools, Mainnet preflight).

2. [Critical] plan line 275 (also 17 and 75) — No step creates a branch. The checkout is on `main`, and every task says "commit", so a literal implementer commits on `main`. A draft pull request cannot be opened from `main`. A push of `main` puts red, unreviewed commits on main and starts "Deploy stage (VPS)" (its push filter has `scripts/**` and `config/**`). Task 6's rules and Alertmanager template would then go live before review. — Fix: add a Step 0 before Task 1: in clutch-deploy run `git fetch origin` and `git switch -c feat/mainnet-treasury-readiness origin/main`, confirm it is still `150b0a5` (or re-check the anchors), and push only that branch.

3. [Important] plan lines 1124-1128 — The check "passes CHAIN to the host" passes too easily. `grep -q 'CHAIN'` already matches the new confirmation step (`CHAIN: ${{ inputs.chain }}`), and `set -a` is already in halt-minting.yml, sweep-address.yml and set-mint-caps.yml. If the `for v in ... CHAIN;` edit is missed, the test stays green, and a run with chain `mainnet` and the word `halt mainnet` acts on STAGE (CHAIN unset means stage). — Fix: replace the condition with
   `if grep -qE "for v in [A-Z_ ]*CHAIN;|printf 'CHAIN=%q" "$f" && grep -q 'set -a' "$f"; then`
   It fails on today's files and passes only when CHAIN is really written to `$R.env`. The count (28) does not change.

4. [Important] plan lines 1210-1232 — Public log, mainnet. `main()` in activate-float.sh prints the whole `/internal/xpub` reply (activate-float.sh lines 75-78). That reply contains `account_xpub` (tron-signer `main.rs:93`). With chain `mainnet` the mainnet xpub goes into a public log, and anyone can derive every user's deposit address from it. This breaks the plan's own rule at line 1853. — Fix: in the same Task 3 edit, change activate-float.sh line 78 to
   `2>/dev/null | sed 's/,/,\n    /g' | grep -v '"account_xpub"' | sed 's/^/    /' || echo "    (could not read /internal/xpub)"`

5. [Important] plan lines 1940-1941 (promise at 1853) — `left(message, 70)` does not hide addresses. Treasury alert texts start with a user's deposit address, for example `sweep of {address} (index {index}) failed` and `swept {address} in {tx_id}` (clutch-treasury `sweeper.rs` lines 341 and 262). A 34-character address fits in 70 characters. — Fix: mask first, then cut:
   `"select severity, source, left(regexp_replace(message, 'T[1-9A-HJ-NP-Za-km-z]{33}', '<address>', 'g'), 70) as message, created_at from alerts order by created_at desc limit 6;"`

6. [Important] plan lines 712-718 (and 662) — Removing `MINT_AUTHORITY_SECRET=unused-this-chain-signs-with-kms` from `.env.mainnet.example` makes a trap. `provision-treasury-secrets.sh` generates every missing name in `GENERATED`, and `MINT_AUTHORITY_SECRET` is in that list (lines 41 and 199, `openssl rand -hex 32`). A new `.env.mainnet` made from the new example and then provisioned gets a 64-hex plaintext key, and the Task 4 preflight then refuses every start. The host's current file still has the placeholder, so this rollout works. — Fix: keep the active line `MINT_AUTHORITY_SECRET=unused-this-chain-signs-with-kms` and change only its comment to: "Not read by docker-compose.mainnet.treasury.yml. It stays so that provision-treasury-secrets.sh does not generate a hex key here; mainnet-treasury-up.sh refuses a hex value."

7. [Important] plan lines 246, 896, 1143 and 697 — Path filters are missing, so some checks do not run on a later change. `test-chain.sh` says it ties the names to the compose files (plan lines 775-776), but `test-treasury-scripts.yml` does not list `docker-compose.treasury.yml` or `docker-compose.mainnet.treasury.yml`. Task 7's self-check reads `.env.mainnet.example`, which is not listed either. `check-monitoring-config.yml` renders `.env.mainnet.example` but does not list it. — Fix: in Task 2 Step 2 add `- "docker-compose.treasury.yml"` and `- "docker-compose.mainnet.treasury.yml"` to both lists of test-treasury-scripts.yml. In Task 7 Step 2 add `- ".env.mainnet.example"` to both lists and add that workflow to the commit's `git add`. In Task 1 Step 6 add `- '.env.mainnet.example'` to the list of check-monitoring-config.yml.

8. [Minor] plan line 1988 — The new `PROBE=metrics` query returns two series, but the loop prints only the last value (`tail -1`, inspect-stage.sh line 997). Rollout step 7 ("both mainnet jobs are up") cannot be read from it. Once the mainnet treasury is scraped, the bare queries in the same loop (`clutch_treasury_up`, `clutch_treasury_clt_liability / 1000000`, ...) also return two series, so the stage numbers there become unclear. — Fix: use `'min(up{job=~"mainnet-treasury-service|mainnet-payment-orchestrator"})'` (1 only when both are up). Later, scope the stage queries with `{chain="testnet"}`.

9. [Minor] plan line 1243 — "the three `grep -E '^(PER_TX|DAILY)_MINT_CAP_CLT=' .env`": set-mint-caps.sh has two (lines 53 and 61). — Fix: write "the two".

10. [Minor] plan line 1143 — `scripts/sweep-address.sh` is already in both path lists (lines 20 and 37) and in `bash -n` (line 59). — Fix: write "(activate-float.sh and sweep-address.sh are already listed)" and do not add it again.

11. [Minor] plan line 30 (Fact 1) — `set-image.sh:34` and `check-monitoring-config.yml:21,82,89` contain the file name `docker-compose.mainnet.treasury.yml`, not the project `clutch-main-treasury`. The project name is only in two comments (overlay line 3, `.env.mainnet.example` line 3). — Fix: correct the sentence. All other facts match the checkout; Fact 3 and Fact 4 are exact.

12. [Minor] plan line 750 — The log line is `all compose files parse` (lower case), not `All compose files parse`. — Fix: quote it exactly.

13. [Minor] plan line 2483 — The new block starts with its own rule line, but the old opening rule (`.env.mainnet.example` line 60) stays, so two rule lines follow each other. — Fix: write "replace lines 60-71".

14. [Minor] plan line 2742 — The two `docker` lines are inside a ```bash fence (ALERTING.md lines 149-152). "Add after it" puts the sentence inside the code block. — Fix: "add a new paragraph after the closing fence (line 152)".

15. [Minor] plan line 1496 — The needle `main mnemonic` is not part of `mainnet mnemonic words`, so it can never find a leak. — Fix: use `mainnet mnemonic`.

16. [Minor] plan line 1738 — `docker logs --tail 30 "$c" 2>&1 | sed ...` runs under `set -euo pipefail`. If `docker logs` fails, the script stops before the ABORT line. — Fix: add `|| true` at the end of that line.

17. [Minor] plan lines 1584 and 2737 — Nothing checks that `.env.mainnet` has its own `BACKUP_PASSPHRASE` and `BACKUP_REMOTE`, but the doc says it does. With the same remote, both chains write `treasury-<stamp>.dump.enc` into one folder, and a restore can take the other chain's dump. — Fix: add `BACKUP_PASSPHRASE BACKUP_REMOTE` to `PF_DIFFER`. They are compared only when set, so the fixtures and the 19 cases do not change.

18. [Minor] plan line 2315 — Running "Set GasFree settings" for mainnet again writes B4's limits again, so a pilot's lower mint caps go back up with no warning. — Fix: add to the Task 8 ON-CALL row: "Running it again resets the limits to B4's values."

19. [Minor] plan lines 1193-1208 — On mainnet, `sweep-address.yml` prints `address: $ADDRESS`, and the script prints that address and its intent rows, into the public log. The dispatch input already shows the address, but this conflicts with "no address of a user". — Fix: say so in the Task 8 ON-CALL text, or keep sweep-address stage-only for now.

20. [Minor] plan lines 2740-2756 (Task 8) — Docs left behind: CLAUDE.md line 98 still says "the mainnet treasury overlay"; the probe lists (CLAUDE.md "Probes:" line, inspect-stage.sh line 5) do not name `mainnet-treasury`; the rule table in ALERTING.md (lines 16-28) has no `TreasuryWatcherCursorStrandedMainnet`. — Fix: add these three edits to Task 8 Step 3.

All stated case counts are right: 15; 11 (9 fail in red); 21 (3 fail); 28; 19; 5 promtool tests (3 fail); 18 (7 fail). Only Task 2's red cannot be seen in the log (item 1). Every other anchor exists exactly once, except items 9, 10, 13 and 14.

Verdict: fix first
