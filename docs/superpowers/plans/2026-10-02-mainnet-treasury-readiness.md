# Mainnet Treasury Readiness (Plan 5) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the mainnet treasury startable, observable and safe to operate in `clutch-deploy`, with the GasFree rail ready to switch on, without a single user able to reach it: a compose file whose names cannot collide with the stage stack, one `CHAIN` switch for the operator tools, a typed start workflow with checks before it starts anything, a probe, monitoring, and a writer for the mainnet settings the maintainer accepted on 2026-10-02.

**Architecture:** One repository, `clutch-deploy`, eight tasks, then a rollout with the maintainer. Task 1 replaces the mainnet overlay with a complete compose file of its own whose three app services have `mainnet-` names (the overlay would have put two containers under one name on shared networks), and adds a CI guard. Task 2 adds `scripts/lib/chain.sh` and makes `check-cap-invariants.sh` read any env file. Task 3 makes the operator scripts and their workflows take `CHAIN`. Task 4 adds the start workflow and its preflight. Task 5 adds `PROBE=mainnet-treasury`. Task 6 adds the Prometheus jobs, the chain-scoped rules and their unit test. Task 7 teaches the settings writer the mainnet values. Task 8 writes the docs and the rollout steps. Nothing here changes what the stage stack does.

**Tech Stack:** Bash, GitHub Actions, Docker Compose, `jq`, Prometheus rules (`promtool test rules`), Alertmanager templates.

**Spec:** `clutch-treasury/docs/mainnet-readiness.md` — the gate, A2, B1, B2, B4, D1, D3, G3 — and `docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md` §9 ("Mainnet follows only after that, with its own API key and its own fee reading"). The plans before this one: Plans 1-4 of the GasFree rail (#49, #51, #53, #54/#55), merged; the Nile rollout ran on 2026-10-02 and is recorded in the spec's §9.

## Global Constraints

- **No local builds.** "Do not run `cargo`, `npm`, `docker`, or any build/test/lint command on this Windows host, and forbid it in every subagent prompt. Verify code by dispatching CI and reading the run log." That includes `bash -n` and `jq`.
- A test counts only when the CI log shows it **by name**. A green badge is not evidence. Pick the CI run by head SHA.
- One implementer per checkout at a time. Commit with `git commit -F <file>`; never put backticks in `-m`. Run git inside the repository, never in `D:\source\clutch`. Files in `clutch-deploy` are stored LF; edit them with the Edit tool, never through a bash heredoc (it eats backslashes). All eight tasks commit on the branch `feat/mainnet-treasury-readiness` (Task 1, Step 0), never on `main`: a push of `main` starts "Deploy stage (VPS)".
- "**`tron-signer`'s SWEEP API takes an INDEX and nothing else** — the destination is its own config. Do not add a `to`, `contract`, or `amount` parameter there." `/internal/activate-float` takes no parameters either; the scripts that call them pass none, for either chain. The payout endpoint "can only spend from the payout float at `2/0` — never a deposit address, never custody".
- "**Never share secrets between `.env` and `.env.mainnet`.**" One `DEPOSIT_MNEMONIC` derives the same TRON addresses on Nile and on mainnet. **Never `down -v`** against `clutch-main` or `clutch-main-treasury`: the chain and the treasury's two databases live there. No workflow in this plan runs `down` at all.
- "Stage secrets live in the host's `.env`, which the deploy workflow READS and never writes." The same holds for `.env.mainnet`: it is written only by `provision-treasury-secrets.yml` and by the reviewed settings writer of Task 7, which never writes or prints a key, a secret, a mnemonic or a token. No script in this plan prints one.
- **This plan opens nothing to users.** `/payment/` on the mainnet site stays at 503 (`config/nginx/clutch.d/app.clutchprotocol.io/payment.conf`), and no mainnet treasury service joins a stage network. Handing a mainnet address to a user needs the readiness gate (A2, B1/B2) and is a later plan.
- **Changes that reach the running stage stack, and nothing else does:** the stage ops scripts keep their exact behaviour when `CHAIN` is unset; `CHAIN` is a new workflow input that defaults to `stage`; Task 6 adds a `chain="testnet"` label to the two stage treasury scrape jobs and scopes one rule to it. Merging runs a normal stage deploy (`scripts/**` and `config/**` change), which the images' pins keep from shipping anything new.
- The numbers the maintainer accepted on 2026-10-02 (every amount in micro-USDT, which is also micro-CLT at par): GasFree maxima `GASFREE_ACTIVATE_FEE_MAX_USDT=2000000` and `GASFREE_TRANSFER_FEE_MAX_USDT=2000000`; `MIN_DEPOSIT_USDT=5000000`; `REDEMPTION_FEE_USDT=2000000`; `MIN_REDEMPTION_CLT=25000000`; `PAYOUT_FLOAT_TARGET_USDT=1000000000`. With them, from readiness B4 (decided 2026-09-17): `PER_TX_MINT_CAP_CLT=1000000000`, `DAILY_MINT_CAP_CLT=2000000000`, `MAX_REDEMPTION_CLT=200000000`, `PER_TX_PAYOUT_CAP_USDT=200000000`, `DAILY_PAYOUT_CAP_CLT=1000000000`. The live mainnet reading of 2026-10-02: USDT `TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t`, `activateFee` 1500000 and `transferFee` 1500000, provider `TLntW9Z59LYY5KEi9cmwk3PKjQga828ird`.
- Commit messages and PR bodies: precise, and the plain word where both words work (workspace CLAUDE.md).

## Facts

Read in `clutch-deploy` `150b0a5` (2026-10-02) and `clutch-treasury` `e93bbb2`. Every task relies on these.

1. **The mainnet treasury is an overlay that nothing runs.** `docker-compose.mainnet.treasury.yml` (113 lines) overlays `docker-compose.treasury.yml` and keeps its service names. Its header (lines 3-5) gives the only start command: `docker compose -p clutch-main-treasury --env-file .env.mainnet -f docker-compose.treasury.yml -f docker-compose.mainnet.treasury.yml up -d treasury-postgres orchestrator-postgres treasury-service tron-signer payment-orchestrator`. No workflow or script runs it; the project name `clutch-main-treasury` appears only in two comments (the overlay's line 3 and `.env.mainnet.example` line 3), and the file name `docker-compose.mainnet.treasury.yml` in `set-image.sh:34` and `check-monitoring-config.yml:21,82,89`.
2. **No compose file sets `container_name`**, so a container is `<project>-<service>-1`. Today that would be `clutch-main-treasury-treasury-service-1`, `...-tron-signer-1`, `...-payment-orchestrator-1`, `...-treasury-postgres-1`, `...-orchestrator-postgres-1`; the volumes are `clutch-main-treasury_treasury-postgres-data` and `clutch-main-treasury_orchestrator-postgres-data`.
3. **The overlay would put two containers under one name.** It joins the orchestrator to `clutch-stage_clutch-network` with the alias `mainnet-payment-orchestrator` (overlay lines 88-94, 111-113), and its comment says the alias keeps the two orchestrators apart. It does not: Compose also gives every container its service name as an alias on each network it joins, so `payment-orchestrator` would then name two containers on the stage network, where nginx's `/payment/` route proxies `payment-orchestrator:8091`. The same orchestrator carries `APP_TREASURY_URL=http://treasury-service:8090` (base line 308) while the stage `treasury-service` sits on that network too. Prometheus is attached to `clutch-network` (stage) and `clutch-mainnet` (`docker-compose.stage.cloudflare-flex.yml:31-33`), so the job target `treasury-service:9101` (`prometheus.yml:109-112`) would resolve to either treasury. The validators (`mainnet-node1`) and the hub (`mainnet-hub-api`) already avoid this with their own names.
4. **The documented start command would publish the orchestrator's API.** The base file has `ports: - "8091:8091"` on `payment-orchestrator` (lines 295-296); stage unpublishes it with `docker-compose.stage.treasury.yml:14-15`, a file the mainnet command does not include.
5. **The overlay pins the old images.** `sha-92d357e` (lines 32, 74, 80) is the build before the GasFree treasury code; stage runs `sha-df5243a`. `set-image.sh` edits `docker-compose.mainnet.yml` and `docker-compose.mainnet.treasury.yml` (lines 33-34) by matching `image: ghcr.io/clutchprotocol/<image>:<tag>` lines (`tags_in`).
6. **What the overlay overrides** (the standalone file of Task 1 must keep each): `APP_NODE_WS_URL=ws://mainnet-node3:8183/ws`, `APP_NODE_PEER_WS_URLS=ws://mainnet-node1:8181/ws,ws://mainnet-node2:8182/ws`, `APP_CHAIN_ID=1000`, `APP_SIGNER_KIND=azure_kms` with the six `APP_AZURE_*` settings (all `:?` required), `APP_MINT_AUTHORITY_SECRET=` empty (the service panics if a plaintext mint key sits beside the KMS signer), `TRONGRID_URL` and `USDT_CONTRACT` required instead of defaulting to Nile, `PAYOUT_FLOAT_ADDRESS` required, the orchestrator's `APP_ALLOWED_ORIGINS=${MAINNET_ALLOWED_ORIGINS:-https://app.clutchprotocol.io}`, and `clutch-network` re-pointed at the external network `clutch-mainnet`. The `clutch-explorer-backend` stub (lines 95-102) exists only because the base treasury file adds an environment line to a service of the base compose; a standalone file does not need it.
7. **`.env.mainnet.example`** sets the caps to stage values (lines 65-71: 50000000, 500000000, 25000000, 25000000, 5000000, `REDEMPTION_FEE_USDT=1000000`, 100000000), keeps `MINT_AUTHORITY_SECRET=unused-this-chain-signs-with-kms` only for the base's interpolation, and has a commented GasFree block (lines 96-111) with the mainnet URL and both reviewed implementations, and the maxima, provider, `MIN_DEPOSIT_USDT` and `PAYOUT_FLOAT_TARGET_USDT` blank. `.gitignore` (lines 2-8) lists `.env`, `.env.local`, `.env.*.local`, `.env.bak` and `.env.bak.*`: nothing matches `.env.mainnet` or the `.env.mainnet.bak` that `provision-treasury-secrets.sh:144` writes.
8. **The operator tools are stage-only.** Each hardcodes `clutch-stage-*` names and `.env`: `halt-minting.sh:41-42`, `resume-minting.sh:15-16`, `set-mint-caps.sh:30-61,67-72,77-78`, `activate-float.sh:30-32`, `sweep-address.sh:30-31`, `backup-treasury-db.sh:29,40,74-75`, `restore-treasury-db.sh:32,37,48-49`, `verify-restored-ledger.sh:32-36,53-54,67,106-111`. Switches that exist: `ENV_FILE` in `provision-treasury-secrets.sh:53`; `TREASURY_CONTAINER` and `ORCHESTRATOR_CONTAINER` in the backup, restore and verify scripts; `check-cap-invariants.sh` takes the process environment before `.env` (line 41) but names `.env` only (lines 42-43). `PROBE=gasfree` already reads `.env.mainnet` (merged as #106); the `treasury`, `sweeper` and `mainnet` probes show nothing of a mainnet treasury (`mainnet` filters on the project `clutch-main`, lines 1037-1042).
9. **How the workflows reach the host.** All SSH to the one stage host (secrets `STAGE_HOST`, `STAGE_USER`, `STAGE_SSH_PASSWORD`, `STAGE_DEPLOY_PATH`), run `git pull --ff-only origin main`, then `bash scripts/<x>.sh`. A workflow with an input passes it as a line of a file of shell-quoted assignments that the host sources (`printf '%s=%q\n'`); the input is never interpolated into the script body (see `set-gasfree-settings.yml`). Confirmation words: `halt`, `resume`, `set`, `activate`, `sweep`, `gasfree`; concurrency groups `treasury-breaker`, `resume-minting`, `mint-caps`, `activate-float`, `sweep-address`, `set-gasfree-settings`, `backup-treasury-db`. `backup-treasury-db.yml` runs on cron `17 3 * * *` with no inputs.
10. **The backup script** reads `BACKUP_PASSPHRASE`, `BACKUP_REMOTE`, `BACKUP_RETAIN` and both database passwords from `.env`, writes `treasury-<stamp>.dump.enc` and `orchestrator-<stamp>.dump.enc` into `${BACKUP_DIR:-backups}`, copies each with `rclone copy ... "$BACKUP_REMOTE"`, and prunes the directory by those two prefixes (lines 111-148). Two chains sharing a directory would prune each other's dumps.
11. **Monitoring.** `prometheus.yml:109-117` has the jobs `treasury-service` (`treasury-service:9101`) and `payment-orchestrator` (`payment-orchestrator:9102`), scraped every 30 s, no labels. The nodes and hubs carry `chain: testnet|mainnet`. `rules/treasury.yml:71` `TreasuryServiceDown` is `up{job=~"treasury-service|payment-orchestrator"} == 0`; `:155` `TreasuryWatcherCursorStranded` is `clutch_treasury_chain_cursor_height > scalar(max(latest_block_index{chain="testnet",role="validator"}))` (its comment, lines 145-147: "A mainnet treasury gets its own rule"); every other treasury rule uses bare `clutch_*` series. The Telegram text (`alertmanager.yml.tpl:88-94`) prints the alert name, severity, summary, description and `instance`. `check-monitoring-config.yml` runs `promtool check config` (image v3.1.0), renders the mainnet treasury merge from `.env.mainnet.example` plus 16 dummy names (lines 73-82), and asserts the KMS signer, the empty plaintext secret and the node URL (lines 90-96); it asserts nothing about job names or a mainnet scrape target.
12. **nginx.** `app.clutchprotocol.io/payment.conf:14-17` answers `/payment/` with `503 {"error":"deposits are not enabled on mainnet yet"}`. There is no upstream for a mainnet orchestrator. `scripts/deploy-stage.sh:467` asserts that 503 at the edge.
13. **The treasury side needs no code change.** The overlay already sets `APP_CHAIN_ID=1000`; the mainnet GasFree chain constants are in `crates/gasfree/src/lib.rs:42-55`; `gasfree::load_settings` turns the rail on by `APP_GASFREE_NETWORK` and accepts `mainnet`.
14. **Live mainnet GasFree reading, 2026-10-02** (`PROBE=gasfree`, #106, run 36995254170): USDT `activateFee` 1500000, `transferFee` 1500000; one provider, `TLntW9Z59LYY5KEi9cmwk3PKjQga828ird`, `maxPendingTransfer` 1, deadlines 60-600 s; the beacon `TSP9UW6FQhT76XD2jWA6ipGMx3yGbjDffP` runs `a3b0edffa1b94e93d297dcc9b6860175e9b537ec` and the controller `TFFAMQLZybALaLb4uxHA9RBE7pxhUAjF3U` runs `c8b13e3104f8a2d6e915ac132bdeda7faaf84d7d`, both the reviewed values. The mainnet key pair is already in the host's `.env.mainnet`.
15. **Readiness state.** A1 (the mint key in Azure KMS) is closed; A2 (the payout key still derived from `DEPOSIT_MNEMONIC` in `tron-signer`'s environment, no KMS signing in `crates/tron-signer`), B1 and B2 (the first real payout receipt, the real fee) are Blockers; C2 and G3 are open by the maintainer's choice. The decided caps are readiness B4's table (decided 2026-09-17); `check-cap-invariants.sh` passed on them with the stage fee.

## Decisions

The spec is the authority; these are where it is silent, or where this plan changes one of its mechanisms, with the reason.

1. **Unique service names, so the mainnet treasury becomes a complete file of its own.** The three app services are `mainnet-treasury-service`, `mainnet-tron-signer` and `mainnet-payment-orchestrator`; the two Postgres services keep their names, because they sit only on the project's private `treasury-network`. A name alias cannot fix Fact 3, since the service name stays an alias. The cost is about 200 lines copied from `docker-compose.treasury.yml`; the old comment's argument against a copy (drift) is answered by a CI guard (Task 1) that fails when a setting of the stage services is missing from the mainnet ones, when a service name is shared, when a mainnet service publishes a port or joins a stage network, or when a mainnet URL names a stage host.
2. **No mainnet treasury service joins a stage network in this plan.** Nothing needs it before go-live: `/payment/` stays 503. When a later plan opens the route it attaches the orchestrator (or a gateway) to the stage network under a name no stage service has, as `mainnet-hub-api` does.
3. **Mainnet starts on the images stage has run the GasFree rollout on:** `sha-df5243a`. The databases are new, so every migration runs from scratch. Moving them later is `set-image.sh mainnet`, a decision of its own.
4. **One switch, `CHAIN=stage|mainnet`, default `stage`**, in `scripts/lib/chain.sh`; every chain-aware workflow gets a `chain` choice input. A mainnet run needs a longer typed word (`<word> mainnet`), so choosing the wrong chain and typing the habitual word is refused.
5. **Required limits on mainnet.** The standalone file requires the seven limits (`PER_TX_MINT_CAP_CLT`, `DAILY_MINT_CAP_CLT`, `DAILY_PAYOUT_CAP_CLT`, `REDEMPTION_FEE_USDT`, `MAX_REDEMPTION_CLT`, `MIN_REDEMPTION_CLT`, `PER_TX_PAYOUT_CAP_USDT`) with `:?` instead of stage's defaults: a default that is silently stage's is a cap nobody chose.
6. **The start is a typed workflow with a pure, tested preflight, and nothing else.** It never runs `down`, has no reset input, and starts the stack with GasFree as `.env.mainnet` says. The preflight refuses a start when a secret equals the stage one, the USDT contract or TronGrid is not mainnet's, the xpubs match, the JWT secrets disagree, or a required name is empty.
7. **A new probe, `PROBE=mainnet-treasury`, instead of parametrising the old ones.** The `treasury` and `sweeper` probes are long and heavily used; a new block cannot break them. It also checks that every treasury service name resolves to exactly one address on each network that matters.
8. **Backups per chain, never mixed.** The mainnet dumps go to `backups/mainnet`, with the passphrase, remote and passwords from `.env.mainnet` (the same script, `CHAIN=mainnet`); the nightly job backs the mainnet up only once its containers exist, and fails when a mainnet treasury that exists cannot be backed up. The restore rehearsal for mainnet is a later step.
9. **Monitoring tells the chains apart by job name and label.** New jobs `mainnet-treasury-service` and `mainnet-payment-orchestrator` carry `chain: mainnet`; the stage jobs get `chain: testnet`. `TreasuryWatcherCursorStranded` is scoped to the testnet and gets a mainnet twin; both are proved by a `promtool test rules` case in which a mainnet cursor above the testnet head does not fire the testnet rule. The Telegram text gains a `chain:` line.
10. **One settings writer for both networks.** For mainnet it writes the GasFree block and the decided limits to `.env.mainnet` together, because `check-cap-invariants.sh` relates them, and ends by running that check on the file it wrote. The values live in the script and are compared with `.env.mainnet.example` by its self-check.
11. **The KMS payout key (A2) and the pilot are separate plans.** This plan ends with the mainnet treasury running, GasFree on, halt, backup and alerts proved, and no user able to reach it. Whether to build A2 before any real USDT, or to run a small pilot with the float capped and the risk recorded, is the maintainer's decision, asked when this plan is done.

## Order of work

| Order | Task | Repo | Needs | Why this order |
|---|---|---|---|---|
| 1 | Task 1: a complete mainnet compose file, and its guard | clutch-deploy | — | Every later task names its containers and services. |
| 2 | Task 2: `chain.sh`, and `check-cap-invariants.sh` for any env file | clutch-deploy | — | Tasks 3, 4 and 7 use both. |
| 3 | Task 3: the operator tools take `CHAIN` | clutch-deploy | 1, 2 | |
| 4 | Task 4: the start workflow and its preflight | clutch-deploy | 1, 2 | |
| 5 | Task 5: `PROBE=mainnet-treasury` | clutch-deploy | 1, 2 | |
| 6 | Task 6: monitoring | clutch-deploy | 1 | |
| 7 | Task 7: the mainnet settings writer | clutch-deploy | 2 | |
| 8 | Task 8: docs, and the rollout with the maintainer | clutch-deploy, then the host | 1-7 | |

Tasks 1-8 all change one checkout, so one implementer runs them one at a time. A task review follows each, and one final whole-branch review follows Task 7. Task 8's rollout changes the host and needs the maintainer; the controller reads the probes.

---

### Task 1: A complete mainnet compose file, and its guard (clutch-deploy)

**Files:**
- Create: `scripts/check-mainnet-compose.sh`
- Create: `scripts/test-check-mainnet-compose.sh`
- Replace: `docker-compose.mainnet.treasury.yml` (a complete file of its own)
- Modify: `.github/workflows/check-monitoring-config.yml` (render both stacks, run the guard, keep the old assertions)
- Modify: `.github/workflows/test-treasury-scripts.yml` (`bash -n`, the self-check, `paths:`)
- Modify: `.gitignore`
- Modify: `.env.mainnet.example` (the start command, the comment on the unused placeholder)

**Interfaces:**
- Consumes: Facts 1-7; Decisions 1, 2, 3 and 5.
- Produces: the services `treasury-postgres`, `orchestrator-postgres`, `mainnet-treasury-service`, `mainnet-tron-signer` and `mainnet-payment-orchestrator` in the compose project `clutch-main-treasury`; the containers `clutch-main-treasury-treasury-postgres-1`, `clutch-main-treasury-orchestrator-postgres-1`, `clutch-main-treasury-mainnet-treasury-service-1`, `clutch-main-treasury-mainnet-tron-signer-1`, `clutch-main-treasury-mainnet-payment-orchestrator-1`; the project-private network `clutch-main-treasury_treasury-network`; `scripts/check-mainnet-compose.sh <stage.json> <mainnet.json>` (exit 0 when every check says `OK`). Tasks 2-7 use these names.

The guard reads two files of `docker compose config --format json` output and nothing else (no docker, no network, no `.env`). Its self-check builds small fixtures with `jq` and mutates them. Nothing here starts a container.

- [ ] **Step 0: The branch**

Every task of this plan commits on one branch of the `clutch-deploy` checkout, never on `main`. A push of `main` starts "Deploy stage (VPS)" (its push filter has `scripts/**` and `config/**`), which would put red, unreviewed commits on the host's checkout, and a draft pull request cannot be opened from `main`. In `D:\source\clutch\clutch-deploy`:

```bash
git fetch origin
git switch -c feat/mainnet-treasury-readiness origin/main
git rev-parse --short HEAD
```

Expected: `150b0a5`. If `git switch -c` says the branch exists already, run `git switch feat/mainnet-treasury-readiness` and check that `git log --oneline origin/main..HEAD` shows only this plan's commits. If `origin/main` has moved past `150b0a5`, read again every anchor the tasks quote (a line number, the text of a line to replace) before editing. Run `git branch --show-current` before and after every commit: it must print `feat/mainnet-treasury-readiness`. Push only that branch, with `git push -u origin feat/mainnet-treasury-readiness`; never `git push origin main`.

- [ ] **Step 1: The self-check, against a stub guard**

Create `scripts/test-check-mainnet-compose.sh`:

```bash
#!/usr/bin/env bash
# Self-check for check-mainnet-compose.sh: each way the mainnet treasury's compose file could be made
# to collide with, or drift from, the stage stack, by exit code and by the line printed. CI runs it
# (test-treasury-scripts.yml) with no docker and no .env: the guard reads two JSON files, and the
# fixtures below are tiny renderings of `docker compose config --format json`.
set -euo pipefail
cd "$(dirname "$0")/.."

T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT

passed=0
failed=0

# The stage stack: the three app services on the stage network, the two databases on a private one.
cat > "$T/stage.json" <<'JSON'
{
  "networks": {
    "clutch-network": {"name": "clutch-stage_clutch-network"},
    "treasury-network": {"name": "clutch-stage_treasury-network", "internal": true}
  },
  "services": {
    "treasury-postgres": {"networks": {"treasury-network": null}, "environment": {"POSTGRES_DB": "treasury"}},
    "orchestrator-postgres": {"networks": {"treasury-network": null}, "environment": {"POSTGRES_DB": "orchestrator"}},
    "treasury-service": {
      "image": "ghcr.io/clutchprotocol/clutch-treasury:sha-df5243a",
      "networks": {"treasury-network": null, "clutch-network": null},
      "environment": {"APP_CHAIN_ID": "2077", "APP_SIGNER_URL": "http://tron-signer:8093", "APP_PER_TX_MINT_CAP_CLT": "50000000"}
    },
    "tron-signer": {
      "image": "ghcr.io/clutchprotocol/clutch-tron-signer:sha-df5243a",
      "networks": {"treasury-network": null, "clutch-network": null},
      "environment": {"APP_PER_TX_PAYOUT_CAP_USDT": "25000000"}
    },
    "payment-orchestrator": {
      "image": "ghcr.io/clutchprotocol/clutch-orchestrator:sha-df5243a",
      "networks": {"treasury-network": null, "clutch-network": null},
      "ports": [{"published": "8091", "target": 8091}],
      "environment": {"APP_TREASURY_URL": "http://treasury-service:8090", "APP_MAX_REDEMPTION_CLT": "25000000"}
    }
  }
}
JSON

# The mainnet stack as it should render: its own names, a private network, no ports, a superset of
# the stage settings plus the KMS ones.
cat > "$T/mainnet.json" <<'JSON'
{
  "name": "clutch-main-treasury",
  "networks": {
    "clutch-network": {"name": "clutch-mainnet", "external": true},
    "treasury-network": {"name": "clutch-main-treasury_treasury-network", "internal": true}
  },
  "services": {
    "treasury-postgres": {"networks": {"treasury-network": null}, "environment": {"POSTGRES_DB": "treasury"}},
    "orchestrator-postgres": {"networks": {"treasury-network": null}, "environment": {"POSTGRES_DB": "orchestrator"}},
    "mainnet-treasury-service": {
      "image": "ghcr.io/clutchprotocol/clutch-treasury:sha-df5243a",
      "networks": {"treasury-network": null, "clutch-network": null},
      "environment": {
        "APP_CHAIN_ID": "1000", "APP_SIGNER_URL": "http://mainnet-tron-signer:8093",
        "APP_PER_TX_MINT_CAP_CLT": "1000000000", "APP_SIGNER_KIND": "azure_kms", "APP_MINT_AUTHORITY_SECRET": "",
        "APP_NODE_WS_URL": "ws://mainnet-node3:8183/ws",
        "APP_USDT_CONTRACT": "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t", "APP_TRONGRID_URL": "https://api.trongrid.io"
      }
    },
    "mainnet-tron-signer": {
      "image": "ghcr.io/clutchprotocol/clutch-tron-signer:sha-df5243a",
      "networks": {"treasury-network": null, "clutch-network": null},
      "environment": {
        "APP_PER_TX_PAYOUT_CAP_USDT": "200000000",
        "APP_USDT_CONTRACT": "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t", "APP_TRONGRID_URL": "https://api.trongrid.io"
      }
    },
    "mainnet-payment-orchestrator": {
      "image": "ghcr.io/clutchprotocol/clutch-orchestrator:sha-df5243a",
      "networks": {"treasury-network": null, "clutch-network": null},
      "environment": {
        "APP_TREASURY_URL": "http://mainnet-treasury-service:8090", "APP_MAX_REDEMPTION_CLT": "200000000",
        "APP_USDT_CONTRACT": "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t", "APP_TRONGRID_URL": "https://api.trongrid.io"
      }
    }
  }
}
JSON

# check <name> <expected exit code> <text the output must contain> <jq filter that mutates the mainnet fixture>
check() {
  local name="$1" want="$2" text="$3" filter="$4" out code=0
  jq "$filter" "$T/mainnet.json" > "$T/m.json"
  out=$(bash scripts/check-mainnet-compose.sh "$T/stage.json" "$T/m.json" 2>&1) || code=$?
  if [ "$code" -eq "$want" ] && printf '%s' "$out" | grep -qF -- "$text"; then
    passed=$((passed + 1))
    echo "ok    $name"
  else
    failed=$((failed + 1))
    echo "FAIL  $name: exit $code (wanted $want), wanted the text: $text"
    printf '%s\n' "$out" | sed 's/^/        /'
  fi
}

check "a clean pair passes" 0 "no service name is shared with the stage stack" '.'
check "the databases may share a name: they sit on a private network" 0 "OK    no service name is shared" '.'
check "an app service with a stage name fails" 1 "service names shared with the stage stack: treasury-service" \
  '.services["treasury-service"] = .services["mainnet-treasury-service"] | del(.services["mainnet-treasury-service"])'
check "a database on a shared network under a stage name fails" 1 "service names shared with the stage stack: treasury-postgres" \
  '.services["treasury-postgres"].networks = {"clutch-network": null}'
check "a published port fails" 1 "services that publish a port: mainnet-payment-orchestrator" \
  '.services["mainnet-payment-orchestrator"].ports = [{"published": "8091", "target": 8091}]'
check "a stage network fails" 1 "services on a stage network: mainnet-payment-orchestrator" \
  '.networks["clutch-stage"] = {"name": "clutch-stage_clutch-network", "external": true} | .services["mainnet-payment-orchestrator"].networks["clutch-stage"] = null'
check "a stage setting missing from the mainnet service fails" 1 "mainnet-tron-signer lacks the stage settings: APP_PER_TX_PAYOUT_CAP_USDT" \
  'del(.services["mainnet-tron-signer"].environment.APP_PER_TX_PAYOUT_CAP_USDT)'
check "a stage host in a mainnet URL fails" 1 "mainnet settings that name a stage host: mainnet-payment-orchestrator.APP_TREASURY_URL" \
  '.services["mainnet-payment-orchestrator"].environment.APP_TREASURY_URL = "http://treasury-service:8090"'
check "an image on latest fails" 1 "not pinned to a sha tag: mainnet-treasury-service" \
  '.services["mainnet-treasury-service"].image = "ghcr.io/clutchprotocol/clutch-treasury:latest"'
check "the wrong chain id fails" 1 "mainnet-treasury-service must run chain 1000" \
  '.services["mainnet-treasury-service"].environment.APP_CHAIN_ID = "2077"'
check "a plaintext mint key beside the KMS signer fails" 1 "mainnet-treasury-service must have an empty APP_MINT_AUTHORITY_SECRET" \
  '.services["mainnet-treasury-service"].environment.APP_MINT_AUTHORITY_SECRET = "abcd"'
check "the env signer instead of KMS fails" 1 "mainnet-treasury-service must sign with azure_kms" \
  '.services["mainnet-treasury-service"].environment.APP_SIGNER_KIND = "env"'
check "the testnet node URL fails" 1 "mainnet-treasury-service must read a mainnet node" \
  '.services["mainnet-treasury-service"].environment.APP_NODE_WS_URL = "ws://node3:8083/ws"'
check "the Nile TronGrid fails" 1 "mainnet-tron-signer must use TronGrid mainnet" \
  '.services["mainnet-tron-signer"].environment.APP_TRONGRID_URL = "https://nile.trongrid.io"'
check "the Nile USDT contract fails" 1 "mainnet-payment-orchestrator must watch the mainnet USDT contract" \
  '.services["mainnet-payment-orchestrator"].environment.APP_USDT_CONTRACT = "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf"'

echo ""
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
```

- [ ] **Step 2: A stub guard**

Create `scripts/check-mainnet-compose.sh` as a stub, so CI shows each case failing by name (it exits 0, so every case that expects a failure fails, and the clean case fails on its text):

```bash
#!/usr/bin/env bash
# Stub: test-check-mainnet-compose.sh runs against this and fails by name. The next commit fills it in.
exit 0
```

Add the self-check to CI. In `.github/workflows/test-treasury-scripts.yml`: add `- "scripts/check-mainnet-compose.sh"` and `- "scripts/test-check-mainnet-compose.sh"` to both `paths:` lists (pull_request and push); add `bash -n scripts/check-mainnet-compose.sh` and `bash -n scripts/test-check-mainnet-compose.sh` after the last `bash -n` line of the `Syntax` step; put the line `        if: ${{ !cancelled() }}` (between `- name:` and `run:`) on each of the four test steps that exist (`Cap invariants`, `GasFree fee check`, `Float activation decision`, `GasFree settings writer`); and add a step after `GasFree settings writer`:

```yaml
      - name: Mainnet compose guard
        if: ${{ !cancelled() }}
        run: bash scripts/test-check-mainnet-compose.sh
```

The `if:` line matters. Without it, GitHub skips every step after a red one, so a later task's red commit would show only its first failing suite, and the controller could not read the other suites' `FAIL` lines by name. Every test step that a later task adds carries the same line.

- [ ] **Step 3: Commit the red state**

Commit message file `commit-msg.txt`:

```text
test: the mainnet compose guard's cases, against a stub

check-mainnet-compose.sh will compare the rendered mainnet treasury
file with the stage stack: a shared service name, a published port, a
stage network, a missing stage setting, a stage host in a URL, an
unpinned image, the wrong chain, signer, node, TronGrid or USDT. This
commit adds its self-check and a stub, so CI shows each case failing
by name first.

Co-Authored-By: Claude <noreply@anthropic.com>
```

```bash
git add scripts/check-mainnet-compose.sh scripts/test-check-mainnet-compose.sh .github/workflows/test-treasury-scripts.yml
git commit -F commit-msg.txt
```

Controller: push, open a draft pull request (CI runs on the pull request), and run CI. Expected: "Test treasury scripts" fails, and the self-check shows `FAIL` by name for all 15 cases (a stub that exits 0 and prints nothing fails the cases that expect a failure on their exit code, and the two cases that expect a pass on their text); the other suites stay green: 17, 8, 10 and 10 passed.

- [ ] **Step 4: The guard**

Replace `scripts/check-mainnet-compose.sh`:

```bash
#!/usr/bin/env bash
#
# The mainnet treasury's compose file against the stage stack's: names that would collide, ports,
# networks and drift. CI runs it on the two rendered configs (check-monitoring-config.yml):
#
#   docker compose --env-file stage.env -f docker-compose.yml -f docker-compose.treasury.yml config --format json > stage.json
#   docker compose --env-file mainnet.env -f docker-compose.mainnet.treasury.yml config --format json > mainnet.json
#   bash scripts/check-mainnet-compose.sh stage.json mainnet.json
#
# Why it exists. Compose gives every container its service name as a DNS alias on every network it
# joins, and an `aliases:` entry only adds to that. Prometheus and nginx sit on networks both stacks
# would join, so a second `treasury-service` or `payment-orchestrator` there is two containers
# answering to one name: scrapes and proxied requests reach either stack at random, and the mainnet
# orchestrator can call the TESTNET treasury. The mainnet file therefore has its own names, and is a
# copy of the money path; this is what keeps the copy from drifting from the original.
#
# It reads JSON only: no docker, no network, no .env.

set -uo pipefail

STAGE="${1:?usage: check-mainnet-compose.sh <stage.json> <mainnet.json>}"
MAIN="${2:?usage: check-mainnet-compose.sh <stage.json> <mainnet.json>}"

fail=0
ok()  { printf 'OK    %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; fail=1; }

# The three app services: stage name, then the mainnet name.
PAIRS="treasury-service:mainnet-treasury-service tron-signer:mainnet-tron-signer payment-orchestrator:mainnet-payment-orchestrator"
MAINNET_USDT=TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t

names() { jq -r '.services | keys[]' "$1" | sort; }

# Services that touch a network that is not private to their own project. A private network is
# `internal: true`: its name is project-scoped, so the same service name in the other stack is a
# different container on a different network and cannot collide.
on_shared_network() {
  jq -r '. as $r | .services | to_entries[] | .key as $s
         | (.value.networks // {} | keys[]) as $n
         | select(($r.networks[$n].internal // false) | not) | $s' "$1" | sort -u
}

# 1. A mainnet service on a shared network may not carry a name the stage stack also uses.
shared=$(comm -12 <(names "$STAGE") <(on_shared_network "$MAIN") | tr '\n' ' ')
if [ -z "$shared" ]; then
  ok "no service name is shared with the stage stack"
else
  bad "service names shared with the stage stack: ${shared% }"
fi

# 2. No mainnet service publishes a port.
published=$(jq -r '.services | to_entries[] | select((.value.ports // []) | length > 0) | .key' "$MAIN" | tr '\n' ' ')
if [ -z "$published" ]; then ok "no mainnet service publishes a port"; else bad "services that publish a port: ${published% }"; fi

# 3. No mainnet service joins a stage network.
onstage=$(jq -r '. as $r | .services | to_entries[] | .key as $s
                 | (.value.networks // {} | keys[]) as $n
                 | select(($r.networks[$n].name // $n) | startswith("clutch-stage")) | $s' "$MAIN" | sort -u | tr '\n' ' ')
if [ -z "$onstage" ]; then ok "no mainnet service joins a stage network"; else bad "services on a stage network: ${onstage% }"; fi

# 4. Every setting a stage service has, its mainnet twin has too (it may have more).
for p in $PAIRS; do
  s="${p%%:*}" m="${p##*:}"
  missing=$(comm -23 \
    <(jq -r --arg s "$s" '.services[$s].environment // {} | keys[]' "$STAGE" | sort) \
    <(jq -r --arg m "$m" '.services[$m].environment // {} | keys[]' "$MAIN" | sort) | tr '\n' ' ')
  if [ -z "$missing" ]; then
    ok "$m has every setting $s has"
  else
    bad "$m lacks the stage settings: ${missing% }"
  fi
done

# 5. No mainnet setting names a stage host.
stagehosts=$(jq -r '.services | to_entries[] | .key as $s | (.value.environment // {} | to_entries[])
                    | select(.value | tostring | test("//(treasury-service|tron-signer|payment-orchestrator)([:/]|$)"))
                    | "\($s).\(.key)"' "$MAIN" | tr '\n' ' ')
if [ -z "$stagehosts" ]; then ok "no mainnet setting names a stage host"; else bad "mainnet settings that name a stage host: ${stagehosts% }"; fi

# 6. The three images are pinned to a sha tag.
unpinned=""
for p in $PAIRS; do
  m="${p##*:}"
  image=$(jq -r --arg m "$m" '.services[$m].image // ""' "$MAIN")
  case "$image" in
    ghcr.io/clutchprotocol/clutch-treasury:sha-???????|ghcr.io/clutchprotocol/clutch-orchestrator:sha-???????|ghcr.io/clutchprotocol/clutch-tron-signer:sha-???????) ;;
    *) unpinned="$unpinned $m" ;;
  esac
done
if [ -z "$unpinned" ]; then ok "the three images are pinned to a sha tag"; else bad "not pinned to a sha tag:$unpinned"; fi

# 7. What makes it mainnet.
env_of() { jq -r --arg m "$1" --arg k "$2" '.services[$m].environment[$k] // ""' "$MAIN"; }
T=mainnet-treasury-service
[ "$(env_of $T APP_CHAIN_ID)" = "1000" ] && ok "$T runs chain 1000" || bad "$T must run chain 1000"
[ "$(env_of $T APP_SIGNER_KIND)" = "azure_kms" ] && ok "$T signs with azure_kms" || bad "$T must sign with azure_kms"
# configuration.rs panics when signer_kind is azure_kms and this is set: a plaintext mint key may
# not sit beside the KMS one.
[ -z "$(env_of $T APP_MINT_AUTHORITY_SECRET)" ] && ok "$T has an empty APP_MINT_AUTHORITY_SECRET" || bad "$T must have an empty APP_MINT_AUTHORITY_SECRET"
case "$(env_of $T APP_NODE_WS_URL)" in
  ws://mainnet-node*) ok "$T reads a mainnet node" ;;
  *) bad "$T must read a mainnet node" ;;
esac
for p in $PAIRS; do
  m="${p##*:}"
  case "$(env_of "$m" APP_TRONGRID_URL)" in
    *nile*|*shasta*|"") bad "$m must use TronGrid mainnet" ;;
    *) ok "$m uses TronGrid mainnet" ;;
  esac
  [ "$(env_of "$m" APP_USDT_CONTRACT)" = "$MAINNET_USDT" ] \
    && ok "$m watches the mainnet USDT contract" || bad "$m must watch the mainnet USDT contract"
done

exit "$fail"
```

- [ ] **Step 5: The compose file**

Replace `docker-compose.mainnet.treasury.yml` with:

```yaml
# The MAINNET treasury: a complete compose file of its own, not an overlay on docker-compose.treasury.yml.
#
#   docker compose -p clutch-main-treasury --env-file .env.mainnet \
#     -f docker-compose.mainnet.treasury.yml up -d
#
# The workflow "Mainnet — start the treasury" runs exactly that, after the checks of
# scripts/mainnet-treasury-up.sh.
#
# WHY IT IS A COPY, AND WHY THREE SERVICES HAVE `mainnet-` NAMES. Compose gives every container its
# service name as a DNS alias on every network it joins, and an `aliases:` entry only adds to that.
# The first version of this stack was an overlay that kept the stage stack's service names.
# Prometheus sits on `clutch-network` (stage) and on `clutch-mainnet`, and nginx on the stage
# network, so a second `treasury-service` or `payment-orchestrator` on either would have been two
# containers answering to one name: scrapes and proxied requests would reach either stack at random,
# and the mainnet orchestrator could have called the TESTNET treasury. Different service names are the
# only fix; the validators (`mainnet-node1`) and the hub (`mainnet-hub-api`) already work that way.
# The price is a second copy of the money path. The answer to drift is a CI guard,
# scripts/check-mainnet-compose.sh: it fails when a setting of the stage services is missing here,
# when a service name is shared with the stage stack, when a service publishes a port or joins a
# stage network, and when a URL here names a stage host.
#
# THE DEPOSIT MNEMONIC MUST NOT BE THE TESTNET'S. TRON addresses are network-agnostic: one mnemonic
# derives the SAME addresses on Nile and on mainnet. Two orchestrators with separate databases would
# hand the same address to different users, so a depositor shown an address by the testnet app could
# send real USDT to it and have this service credit somebody else. provision-treasury-secrets.sh
# generates one per env file and never overwrites; scripts/mainnet-treasury-up.sh refuses to start
# when any secret here equals the stage one.
#
# NETWORK POSTURE (as the stage stack, with the names changed):
#   mainnet-treasury-service      PRIVATE. Signs mints with the Azure KMS key. No published ports.
#   mainnet-tron-signer           PRIVATE, and the most sensitive: it holds the deposit mnemonic.
#   mainnet-payment-orchestrator  The public API once a route exists. Holds no key; only the xpub.
#                                 NO port is published and it joins NO stage network: nothing
#                                 reaches it from outside, and /payment/ on the mainnet site answers
#                                 503. A later plan attaches it to the stage network, under a name no
#                                 stage service has, when mainnet opens to users.
#   treasury-network              internal: true, private to this project. Both Postgres instances
#                                 live here and nowhere else.
#
# Every secret comes from .env.mainnet. The images are pinned HERE, as the stage file pins its own:
# moving stage onto new treasury code must never move mainnet. Move these with `set-image.sh mainnet`,
# as a decision of its own.

name: clutch-main-treasury

services:
  treasury-postgres:
    image: postgres:16-alpine
    environment:
      POSTGRES_DB: treasury
      POSTGRES_USER: treasury
      POSTGRES_PASSWORD: ${TREASURY_POSTGRES_PASSWORD:?set TREASURY_POSTGRES_PASSWORD in .env.mainnet}
    volumes:
      - treasury-postgres-data:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U treasury"]
      interval: 10s
      timeout: 5s
      retries: 10
    restart: unless-stopped
    networks:
      - treasury-network

  orchestrator-postgres:
    image: postgres:16-alpine
    environment:
      POSTGRES_DB: orchestrator
      POSTGRES_USER: orchestrator
      POSTGRES_PASSWORD: ${ORCHESTRATOR_POSTGRES_PASSWORD:?set ORCHESTRATOR_POSTGRES_PASSWORD in .env.mainnet}
    volumes:
      - orchestrator-postgres-data:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U orchestrator"]
      interval: 10s
      timeout: 5s
      retries: 10
    restart: unless-stopped
    networks:
      - treasury-network

  # PRIVATE ZONE. Deliberately has no `ports:` — adding one exposes the mint authority.
  mainnet-treasury-service:
    image: ghcr.io/clutchprotocol/clutch-treasury:sha-df5243a
    depends_on:
      treasury-postgres:
        condition: service_healthy
    environment:
      - RUST_LOG=${TREASURY_LOG_LEVEL:-info}
      - APP_DATABASE_URL=postgres://treasury:${TREASURY_POSTGRES_PASSWORD}@treasury-postgres:5432/treasury
      # The MAINNET chain: chain 1000, read through node3 and checked against node1 and node2.
      - APP_NODE_WS_URL=ws://mainnet-node3:8183/ws
      - APP_NODE_PEER_WS_URLS=ws://mainnet-node1:8181/ws,ws://mainnet-node2:8182/ws
      - APP_CHAIN_ID=1000

      # The KMS signer, which is the entire point of the key ceremony. Its address is the chain's
      # genesis-committed mint_authority, so nothing else can mint here.
      - APP_SIGNER_KIND=azure_kms
      - "APP_AZURE_TENANT_ID=${AZURE_TENANT_ID:?set AZURE_TENANT_ID in .env.mainnet}"
      - "APP_AZURE_CLIENT_ID=${AZURE_CLIENT_ID:?set AZURE_CLIENT_ID in .env.mainnet}"
      - "APP_AZURE_CLIENT_SECRET=${AZURE_CLIENT_SECRET:?set AZURE_CLIENT_SECRET in .env.mainnet}"
      - "APP_AZURE_VAULT_URL=${AZURE_VAULT_URL:?set AZURE_VAULT_URL in .env.mainnet}"
      - "APP_AZURE_KEY_NAME=${AZURE_KEY_NAME:?set AZURE_KEY_NAME in .env.mainnet}"
      # Pinned, never "latest": Key Vault lets a key grow new versions, and floating would let the
      # mint authority's address change under this service without anyone deciding that.
      - "APP_AZURE_KEY_VERSION=${AZURE_KEY_VERSION:?set AZURE_KEY_VERSION in .env.mainnet}"
      # EMPTY ON PURPOSE. configuration.rs panics when signer_kind=azure_kms and this is non-empty,
      # so a plaintext mint key cannot sit beside the KMS one and be picked up by accident.
      - APP_MINT_AUTHORITY_SECRET=

      - APP_INITIATOR_TOKEN=${TREASURY_INITIATOR_TOKEN:?set TREASURY_INITIATOR_TOKEN in .env.mainnet}
      - APP_APPROVER_TOKEN=${TREASURY_APPROVER_TOKEN:?set TREASURY_APPROVER_TOKEN in .env.mainnet}
      - APP_READONLY_TOKEN=${TREASURY_READONLY_TOKEN:?set TREASURY_READONLY_TOKEN in .env.mainnet}
      # TRON MAINNET. Required, never defaulted: a default that is wrong in this pair means watching
      # the wrong token on the wrong network, and crediting nothing.
      - "APP_TRONGRID_URL=${TRONGRID_URL:?set TRONGRID_URL to https://api.trongrid.io in .env.mainnet}"
      - APP_TRONGRID_API_KEY=${TRONGRID_API_KEY:-}
      - APP_CUSTODY_TRON_ADDRESS=${CUSTODY_TRON_ADDRESS:?set CUSTODY_TRON_ADDRESS in .env.mainnet}
      - "APP_USDT_CONTRACT=${USDT_CONTRACT:?set USDT_CONTRACT in .env.mainnet — verify it on tronscan.org first}"
      # Sweeps: this service decides WHEN, mainnet-tron-signer knows HOW and owns the keys.
      - APP_SIGNER_URL=http://mainnet-tron-signer:8093
      - APP_SIGNER_TOKEN=${SIGNER_TOKEN:?set SIGNER_TOKEN in .env.mainnet}
      # The limits are REQUIRED here (the stage file defaults them): a default that is silently
      # stage's is a cap nobody chose. Readiness B4 has the values and what each one bounds;
      # set-gasfree-settings.sh writes the decided set.
      - APP_PER_TX_MINT_CAP_CLT=${PER_TX_MINT_CAP_CLT:?set PER_TX_MINT_CAP_CLT in .env.mainnet}
      - APP_DAILY_MINT_CAP_CLT=${DAILY_MINT_CAP_CLT:?set DAILY_MINT_CAP_CLT in .env.mainnet}
      - APP_SWEEP_THRESHOLD_USDT=${SWEEP_THRESHOLD_USDT:-100000000}
      - APP_SWEEP_MAX_AGE_HOURS=${SWEEP_MAX_AGE_HOURS:-168}
      - APP_SWEEP_MIN_USDT=${SWEEP_MIN_USDT:-5000000}
      # The plain payout float at 2/0 of THIS deployment's wallet, which provision-treasury-secrets.sh
      # writes from the signer's own /internal/xpub. The GasFree float is derived from this one
      # (treasury-service's AppConfig::gasfree_float).
      - "APP_PAYOUT_FLOAT_ADDRESS=${PAYOUT_FLOAT_ADDRESS:?set PAYOUT_FLOAT_ADDRESS in .env.mainnet — provision-treasury-secrets.sh writes it}"
      # Rolling 24h payout ceiling in CLT BASE UNITS; not the unit of the signer's per-transaction cap.
      - APP_DAILY_PAYOUT_CAP_CLT=${DAILY_PAYOUT_CAP_CLT:?set DAILY_PAYOUT_CAP_CLT in .env.mainnet}
      # Taken off the USDT leg of every redemption, in micro-USDT. On the GasFree rail it must cover
      # the relay's transfer fee at its maximum (check-cap-invariants.sh).
      - APP_REDEMPTION_FEE_USDT=${REDEMPTION_FEE_USDT:?set REDEMPTION_FEE_USDT in .env.mainnet}
      # Every hour, not the service's default 24 h: TreasuryReconciliationStale expects a run within 2 h.
      - APP_RECONCILIATION_INTERVAL_SECS=${RECONCILIATION_INTERVAL_SECS:-3600}
      # The GasFree rail: the SAME .env names as the stage file, so the three services cannot read
      # different values. OFF while GASFREE_NETWORK is blank.
      - APP_TRANSFER_RAIL=${TRANSFER_RAIL:-trx}
      - APP_GASFREE_NETWORK=${GASFREE_NETWORK:-}
      - APP_GASFREE_ACTIVATE_FEE_MAX_USDT=${GASFREE_ACTIVATE_FEE_MAX_USDT:-}
      - APP_GASFREE_TRANSFER_FEE_MAX_USDT=${GASFREE_TRANSFER_FEE_MAX_USDT:-}
      - APP_MIN_DEPOSIT_USDT=${MIN_DEPOSIT_USDT:-}
      - APP_GASFREE_EXPECTED_IMPLEMENTATION=${GASFREE_EXPECTED_IMPLEMENTATION:-}
      - APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION=${GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION:-}
    restart: unless-stopped
    networks:
      - treasury-network   # its own Postgres
      - clutch-network     # the mainnet chain's WebSocket RPC, and TronGrid

  # PRIVATE ZONE, and the most sensitive thing in the stack: this holds the deposit-wallet MNEMONIC,
  # which can spend every derived deposit address. No `ports:`, ever. Reachable only from
  # mainnet-treasury-service over treasury-network. It sits on clutch-network too only because
  # building and broadcasting a sweep needs TronGrid, and `internal: true` has no outbound route.
  # What keeps that acceptable is the API shape: /internal/sweep takes an INDEX and nothing else.
  mainnet-tron-signer:
    image: ghcr.io/clutchprotocol/clutch-tron-signer:sha-df5243a
    environment:
      - RUST_LOG=${SIGNER_LOG_LEVEL:-info}
      - APP_DEPOSIT_MNEMONIC=${DEPOSIT_MNEMONIC:?set DEPOSIT_MNEMONIC in .env.mainnet}
      - APP_DEPOSIT_PASSPHRASE=${DEPOSIT_PASSPHRASE:-}
      - APP_SIGNER_TOKEN=${SIGNER_TOKEN:?set SIGNER_TOKEN in .env.mainnet}
      # Where every sweep goes. NOT a request parameter.
      - APP_TREASURY_ADDRESS=${CUSTODY_TRON_ADDRESS:?set CUSTODY_TRON_ADDRESS in .env.mainnet}
      - "APP_TRONGRID_URL=${TRONGRID_URL:?set TRONGRID_URL in .env.mainnet}"
      - APP_TRONGRID_API_KEY=${TRONGRID_API_KEY:-}
      - "APP_USDT_CONTRACT=${USDT_CONTRACT:?set USDT_CONTRACT in .env.mainnet}"
      # The most one redemption payout may move, in MICRO-USDT: required, and equal to the
      # orchestrator's MAX_REDEMPTION_CLT (check-cap-invariants.sh).
      - APP_PER_TX_PAYOUT_CAP_USDT=${PER_TX_PAYOUT_CAP_USDT:?set PER_TX_PAYOUT_CAP_USDT in .env.mainnet}
      # The GasFree rail. This service turns it on by GASFREE_API_KEY, the other two by GASFREE_NETWORK:
      # key without network, and it refuses to start. The key and secret are relay credentials.
      - APP_TRANSFER_RAIL=${TRANSFER_RAIL:-trx}
      - APP_GASFREE_NETWORK=${GASFREE_NETWORK:-}
      - APP_GASFREE_API_URL=${GASFREE_API_URL:-}
      - APP_GASFREE_API_KEY=${GASFREE_API_KEY:-}
      - APP_GASFREE_API_SECRET=${GASFREE_API_SECRET:-}
      - APP_GASFREE_SERVICE_PROVIDER=${GASFREE_SERVICE_PROVIDER:-}
      - APP_GASFREE_ACTIVATE_FEE_MAX_USDT=${GASFREE_ACTIVATE_FEE_MAX_USDT:-}
      - APP_GASFREE_TRANSFER_FEE_MAX_USDT=${GASFREE_TRANSFER_FEE_MAX_USDT:-}
      - APP_GASFREE_EXPECTED_IMPLEMENTATION=${GASFREE_EXPECTED_IMPLEMENTATION:-}
      - APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION=${GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION:-}
      # Sweeps pay into the GasFree float until it holds this much, then into custody.
      - APP_PAYOUT_FLOAT_TARGET_USDT=${PAYOUT_FLOAT_TARGET_USDT:-}
      - APP_HTTP_ADDR=0.0.0.0:8093
    restart: unless-stopped
    networks:
      - treasury-network   # mainnet-treasury-service reaches it here
      - clutch-network     # TronGrid egress only

  # The public API once a route exists, and not before: it publishes no port and joins no stage network.
  mainnet-payment-orchestrator:
    image: ghcr.io/clutchprotocol/clutch-orchestrator:sha-df5243a
    depends_on:
      orchestrator-postgres:
        condition: service_healthy
    environment:
      - RUST_LOG=${ORCHESTRATOR_LOG_LEVEL:-info}
      - APP_DATABASE_URL=postgres://orchestrator:${ORCHESTRATOR_POSTGRES_PASSWORD}@orchestrator-postgres:5432/orchestrator
      # The secret the MAINNET hub API signs user JWTs with (.env's MAINNET_JWT_SECRET), copied into
      # this file's JWT_SECRET. mainnet-treasury-up.sh refuses a start when the two differ.
      - "APP_JWT_SECRET=${JWT_SECRET:?set JWT_SECRET in .env.mainnet — it must equal MAINNET_JWT_SECRET from .env. Run provision-treasury-secrets.yml}"
      - APP_ALLOWED_ORIGINS=${MAINNET_ALLOWED_ORIGINS:-https://app.clutchprotocol.io}
      - APP_TREASURY_URL=http://mainnet-treasury-service:8090
      - APP_TREASURY_INITIATOR_TOKEN=${TREASURY_INITIATOR_TOKEN:?set TREASURY_INITIATOR_TOKEN in .env.mainnet}
      - APP_TREASURY_READONLY_TOKEN=${TREASURY_READONLY_TOKEN:?set TREASURY_READONLY_TOKEN in .env.mainnet}
      - APP_CUSTODY_TRON_ADDRESS=${CUSTODY_TRON_ADDRESS:?set CUSTODY_TRON_ADDRESS in .env.mainnet}
      # PUBLIC material by design: an account-level xpub derives receive addresses and nothing else.
      - APP_DEPOSIT_ACCOUNT_XPUB=${DEPOSIT_ACCOUNT_XPUB:?set DEPOSIT_ACCOUNT_XPUB in .env.mainnet}
      - "APP_TRONGRID_URL=${TRONGRID_URL:?set TRONGRID_URL in .env.mainnet}"
      - APP_TRONGRID_API_KEY=${TRONGRID_API_KEY:-}
      - "APP_USDT_CONTRACT=${USDT_CONTRACT:?set USDT_CONTRACT in .env.mainnet}"
      # What a user may ask to redeem. The stage file has these ON; here they are OFF until the
      # maintainer turns mainnet on for users, which is a reviewed commit that changes this line.
      - APP_MAX_REDEMPTION_CLT=${MAX_REDEMPTION_CLT:?set MAX_REDEMPTION_CLT in .env.mainnet}
      - APP_MIN_REDEMPTION_CLT=${MIN_REDEMPTION_CLT:?set MIN_REDEMPTION_CLT in .env.mainnet}
      - APP_REDEMPTIONS_ENABLED=false
      - APP_DEPOSIT_HOT_WINDOW_HOURS=${DEPOSIT_HOT_WINDOW_HOURS:-24}
      - APP_PERMANENT_DEPOSIT_ADDRESSES_ENABLED=${PERMANENT_DEPOSIT_ADDRESSES_ENABLED:-true}
      - APP_TRANSFER_RAIL=${TRANSFER_RAIL:-trx}
      - APP_GASFREE_NETWORK=${GASFREE_NETWORK:-}
      - APP_GASFREE_ACTIVATE_FEE_MAX_USDT=${GASFREE_ACTIVATE_FEE_MAX_USDT:-}
      - APP_GASFREE_TRANSFER_FEE_MAX_USDT=${GASFREE_TRANSFER_FEE_MAX_USDT:-}
      - APP_MIN_DEPOSIT_USDT=${MIN_DEPOSIT_USDT:-}
      - APP_GASFREE_EXPECTED_IMPLEMENTATION=${GASFREE_EXPECTED_IMPLEMENTATION:-}
      - APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION=${GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION:-}
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "curl", "-f", "http://127.0.0.1:8091/health"]
      interval: 30s
      timeout: 10s
      retries: 3
      start_period: 20s
    networks:
      - treasury-network   # reach the treasury's internal API
      - clutch-network     # TronGrid

networks:
  treasury-network:
    driver: bridge
    # No route off this network. Both Postgres instances live here and nowhere else.
    internal: true
  # The mainnet chain's network, created by docker-compose.mainnet.yml. External: this project never
  # creates it, and never joins a stage network.
  clutch-network:
    name: clutch-mainnet
    external: true

volumes:
  treasury-postgres-data:
    driver: local
  orchestrator-postgres-data:
    driver: local
```

Three notes on it, all deliberate: `APP_REDEMPTIONS_ENABLED=false` is a fixed value, so turning redemptions on for mainnet is a reviewed commit (the stage file's `true` was one); the explorer stub of the old overlay is gone, because it existed only to satisfy the base treasury file's extra environment line; and `MINT_AUTHORITY_SECRET` is no longer read by this file. Its placeholder line stays in `.env.mainnet.example` for one reason: `provision-treasury-secrets.sh` generates a 64-hex key for every name that is missing, and the preflight (Task 4) refuses a hex value, so a new `.env.mainnet` without the line would never start.

- [ ] **Step 6: The CI render, and the file edits**

In `.github/workflows/check-monitoring-config.yml`, in the step that renders the compose files, replace the whole block from the comment `# The mainnet treasury overlay, merged the way the host merges it` down to and including the line `echo "merge asserted: KMS signer, empty plaintext secret, mainnet node URL"` with:

```yaml
          # The mainnet treasury, rendered the way the host renders it: its own file and --env-file,
          # which is what keeps its secrets separate from the testnet's. `config -q` proves it parses,
          # and the guard below proves what a parse cannot: the KMS signer, the chain, no shared name,
          # no published port, no stage network, no setting lost from the stage services.
          echo "--- mainnet treasury ---"
          sed -e 's/#.*//' -e '/^[[:space:]]*$/d' .env.mainnet.example > /tmp/t.env
          for v in CUSTODY_TRON_ADDRESS TRONGRID_API_KEY AZURE_TENANT_ID AZURE_CLIENT_ID \
                   AZURE_CLIENT_SECRET AZURE_KEY_VERSION DEPOSIT_MNEMONIC DEPOSIT_ACCOUNT_XPUB \
                   SIGNER_TOKEN TREASURY_INITIATOR_TOKEN TREASURY_APPROVER_TOKEN \
                   TREASURY_READONLY_TOKEN TREASURY_POSTGRES_PASSWORD \
                   ORCHESTRATOR_POSTGRES_PASSWORD JWT_SECRET PAYOUT_FLOAT_ADDRESS; do
            echo "$v=ci" >> /tmp/t.env
          done
          docker compose --env-file /tmp/t.env -f docker-compose.mainnet.treasury.yml config -q
          docker compose --env-file /tmp/t.env -f docker-compose.mainnet.treasury.yml config --format json > /tmp/mainnet.json

          # The stage side of the comparison: the base and its treasury file, as the GasFree step renders them.
          {
            for v in JWT_SECRET GRAFANA_ADMIN_PASSWORD TREASURY_POSTGRES_PASSWORD ORCHESTRATOR_POSTGRES_PASSWORD \
                     MINT_AUTHORITY_SECRET TREASURY_INITIATOR_TOKEN TREASURY_APPROVER_TOKEN TREASURY_READONLY_TOKEN \
                     SIGNER_TOKEN DEPOSIT_MNEMONIC CUSTODY_TRON_ADDRESS DEPOSIT_ACCOUNT_XPUB; do
              echo "$v=ci"
            done
          } > /tmp/s.env
          docker compose --env-file /tmp/s.env -f docker-compose.yml -f docker-compose.treasury.yml config --format json > /tmp/stage.json
          bash scripts/check-mainnet-compose.sh /tmp/stage.json /tmp/mainnet.json
```

and add `- 'scripts/check-mainnet-compose.sh'` and `- '.env.mainnet.example'` to its `paths:` list (it has one, under `pull_request:` only, and quotes with single quotes; the workflow renders `.env.mainnet.example`, so a change to that file must run it). In `.gitignore`, after the `.env.bak.*` line, add:

```text
.env.mainnet
.env.mainnet.bak
.env.mainnet.bak.*
```

In `.env.mainnet.example`, replace lines 3-4 (`#   docker compose -p clutch-main-treasury --env-file .env.mainnet \` and `#     -f docker-compose.treasury.yml -f docker-compose.mainnet.treasury.yml up -d ...`) with:

```text
#   docker compose -p clutch-main-treasury --env-file .env.mainnet \
#     -f docker-compose.mainnet.treasury.yml up -d
```

and replace the four comment lines above `MINT_AUTHORITY_SECRET` (lines 51-54, `# Unused, and it must stay that way. ...` through `# ... a non-hex one fails closed if the overlay is dropped.`) with the four lines below. **Keep the line `MINT_AUTHORITY_SECRET=unused-this-chain-signs-with-kms` exactly as it is** (line 55). Four lines replace four, so the later line numbers do not move.

```text
# Not read by docker-compose.mainnet.treasury.yml, which sets APP_MINT_AUTHORITY_SECRET to empty.
# The line stays because provision-treasury-secrets.sh generates a 64-hex key for every name that is
# missing, and scripts/mainnet-treasury-up.sh refuses to start when this value is hex. A plaintext
# mint key does not belong in this file. Leave the placeholder as it is.
```

- [ ] **Step 7: Commit the green state**

Commit message file `commit-msg.txt`:

```text
feat: the mainnet treasury is a complete compose file with its own names

The old overlay kept the stage services' names and joined the stage
network under an alias. Compose also adds the service name as an alias,
so a second payment-orchestrator would have answered next to the stage
one where nginx proxies /payment/, the mainnet orchestrator could have
called the testnet treasury, and Prometheus would have scraped either
treasury at random. It also would have published port 8091.

The mainnet file now stands alone: mainnet-treasury-service,
mainnet-tron-signer and mainnet-payment-orchestrator, no published
port, no stage network, the limits required instead of defaulted,
redemptions off, the images pinned at sha-df5243a. check-mainnet-compose.sh
runs in CI and fails on a shared name, a port, a stage network, a stage
setting missing here, a stage host in a URL, an unpinned image, or the
wrong chain, signer, node, TronGrid or USDT. .env.mainnet is now ignored.

Co-Authored-By: Claude <noreply@anthropic.com>
```

```bash
git add scripts/check-mainnet-compose.sh docker-compose.mainnet.treasury.yml .github/workflows/check-monitoring-config.yml .gitignore .env.mainnet.example
git commit -F commit-msg.txt
```

Controller: push and run CI. Expected: "Test treasury scripts" succeeds and the self-check says `ok` by name for all 15 cases (`15 passed, 0 failed`); "Check monitoring config" succeeds, its log shows `OK` for the guard's lines (every `OK` line of Step 4 for all three pairs) and `all compose files parse`; "Test image pinning" still succeeds (it finds the three mainnet images pinned in the new file; if it names the removed explorer stub, read its log and change the test, not the file — the stub existed only for the old overlay).

### Task 2: One `CHAIN` switch, and an invariants check for any env file (clutch-deploy)

**Files:**
- Create: `scripts/lib/chain.sh`
- Create: `scripts/test-chain.sh`
- Modify: `scripts/check-cap-invariants.sh` (`ENV_FILE`)
- Modify: `scripts/test-cap-invariants.sh` (four cases)
- Modify: `.github/workflows/test-treasury-scripts.yml` (`bash -n`, the self-check, `paths:`)

**Interfaces:**
- Consumes: Task 1's container and service names; Fact 8.
- Produces: `chain_select <stage|mainnet>` setting `CH_NAME`, `CH_ENV_FILE`, `CH_PROJECT`, `CH_SVC_TREASURY`, `CH_SVC_SIGNER`, `CH_SVC_ORCH`, `CH_TREASURY`, `CH_SIGNER`, `CH_ORCH`, `CH_TREASURY_PG`, `CH_ORCH_PG`, `CH_BACKUP_DIR`; `chain_compose_args` (one argument per line); `chain_compose <args>` (runs `docker compose` with them). `ENV_FILE=<file> bash scripts/check-cap-invariants.sh` reads that file instead of `.env`. Tasks 3, 4, 5 and 7 use these.

Decision 4. The stage values are exactly the names the scripts hardcoded before: with `CHAIN` unset, nothing changes. The helper is pure (it runs nothing but `docker compose` inside `chain_compose`), so its self-check needs no docker.

- [ ] **Step 1: The self-checks, against a stub**

Create `scripts/test-chain.sh`:

```bash
#!/usr/bin/env bash
# Self-check for scripts/lib/chain.sh: the container, service, project and file names each chain
# resolves to. The stage names are the ones the operator scripts hardcoded before the helper existed;
# the mainnet names are the ones docker-compose.mainnet.treasury.yml produces, and two cases tie them to
# the compose files so a rename in one place fails here. CI runs it (test-treasury-scripts.yml); it
# needs no docker.
set -euo pipefail
cd "$(dirname "$0")/.."

passed=0
failed=0

# check <name> <wanted> <got>
check() {
  if [ "$2" = "$3" ]; then
    passed=$((passed + 1))
    echo "ok    $1"
  else
    failed=$((failed + 1))
    echo "FAIL  $1"
    printf '        wanted: %s\n        got:    %s\n' "$2" "$3"
  fi
}

# Every variable chain_select sets, on one line. A refused chain prints REFUSED.
names_of() {
  (
    . scripts/lib/chain.sh
    chain_select "$1" >/dev/null 2>&1 || { echo REFUSED; exit 0; }
    echo "$CH_NAME|$CH_ENV_FILE|$CH_PROJECT|$CH_SVC_TREASURY|$CH_SVC_SIGNER|$CH_SVC_ORCH|$CH_TREASURY|$CH_SIGNER|$CH_ORCH|$CH_TREASURY_PG|$CH_ORCH_PG|$CH_BACKUP_DIR"
  )
}
containers_of() {
  (
    . scripts/lib/chain.sh
    chain_select "$1" >/dev/null
    printf '%s\n' "$CH_TREASURY" "$CH_SIGNER" "$CH_ORCH" "$CH_TREASURY_PG" "$CH_ORCH_PG"
  )
}
derived_of() {
  (
    . scripts/lib/chain.sh
    chain_select "$1" >/dev/null
    echo "$CH_PROJECT-$CH_SVC_TREASURY-1 $CH_PROJECT-$CH_SVC_SIGNER-1 $CH_PROJECT-$CH_SVC_ORCH-1"
  )
}
actual_of() {
  (
    . scripts/lib/chain.sh
    chain_select "$1" >/dev/null
    echo "$CH_TREASURY $CH_SIGNER $CH_ORCH"
  )
}
args_of() {
  (
    . scripts/lib/chain.sh
    chain_select "$1" >/dev/null
    chain_compose_args | tr '\n' ' '
  )
}

STAGE="stage|.env|clutch-stage|treasury-service|tron-signer|payment-orchestrator|clutch-stage-treasury-service-1|clutch-stage-tron-signer-1|clutch-stage-payment-orchestrator-1|clutch-stage-treasury-postgres-1|clutch-stage-orchestrator-postgres-1|backups"
MAINNET="mainnet|.env.mainnet|clutch-main-treasury|mainnet-treasury-service|mainnet-tron-signer|mainnet-payment-orchestrator|clutch-main-treasury-mainnet-treasury-service-1|clutch-main-treasury-mainnet-tron-signer-1|clutch-main-treasury-mainnet-payment-orchestrator-1|clutch-main-treasury-treasury-postgres-1|clutch-main-treasury-orchestrator-postgres-1|backups/mainnet"

check "stage: the names the scripts used to hardcode" "$STAGE" "$(names_of stage)"
check "stage is the default" "$STAGE" "$(names_of '')"
check "mainnet: the names docker-compose.mainnet.treasury.yml produces" "$MAINNET" "$(names_of mainnet)"
check "an unknown chain is refused" "REFUSED" "$(names_of testnet)"
check "stage container names are <project>-<service>-1" "$(derived_of stage)" "$(actual_of stage)"
check "mainnet container names are <project>-<service>-1" "$(derived_of mainnet)" "$(actual_of mainnet)"

missing=""
for s in treasury-postgres orchestrator-postgres mainnet-treasury-service mainnet-tron-signer mainnet-payment-orchestrator; do
  grep -q "^  $s:" docker-compose.mainnet.treasury.yml || missing="$missing $s"
done
check "every mainnet service is in docker-compose.mainnet.treasury.yml" "" "$missing"

missing=""
for s in treasury-postgres orchestrator-postgres treasury-service tron-signer payment-orchestrator; do
  grep -q "^  $s:" docker-compose.treasury.yml || missing="$missing $s"
done
check "every stage service is in docker-compose.treasury.yml" "" "$missing"

overlap=$(comm -12 <(containers_of stage | sort) <(containers_of mainnet | sort) | tr '\n' ' ')
check "no mainnet container name is a stage container name" "" "$overlap"

check "stage compose arguments" "-p clutch-stage -f docker-compose.yml -f docker-compose.treasury.yml -f docker-compose.stage.cloudflare-flex.yml -f docker-compose.stage.treasury.yml " "$(args_of stage)"
check "mainnet compose arguments" "-p clutch-main-treasury --env-file .env.mainnet -f docker-compose.mainnet.treasury.yml " "$(args_of mainnet)"

echo ""
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
```

In `scripts/test-cap-invariants.sh`, after the last existing `check` line, add (the file's `check` runs the copied checker from a temp directory that has no `.env`, with the values given as `NAME=value` arguments; these cases write env files into that directory first):

```bash
# The checker reads the file ENV_FILE names, not only .env. The mainnet limits (readiness B4, with
# the $2.00 redemption fee the maintainer accepted on 2026-10-02) live in .env.mainnet.
printf '%s\n' PER_TX_MINT_CAP_CLT=1000000000 DAILY_MINT_CAP_CLT=2000000000 MAX_REDEMPTION_CLT=200000000 \
  MIN_REDEMPTION_CLT=25000000 PER_TX_PAYOUT_CAP_USDT=200000000 REDEMPTION_FEE_USDT=2000000 > "$T/mainnet.env"
check "ENV_FILE names another file: its limits are read" 0 "fee is 8% of the smallest allowed redemption" ENV_FILE=mainnet.env
printf '%s\n' REDEMPTION_FEE_USDT=1000000 MIN_REDEMPTION_CLT=5000000 > "$T/.env"
check "ENV_FILE wins over .env" 0 "fee is 8% of the smallest allowed redemption" ENV_FILE=mainnet.env
check "without ENV_FILE the checker still reads .env" 0 "fee is 20% of the smallest allowed redemption"
rm -f "$T/.env"
printf '%s\n' GASFREE_API_KEY=key-marker-7f3a > "$T/keyonly.env"
check "a key without the network in ENV_FILE is refused" 1 "GASFREE_API_KEY is set while GASFREE_NETWORK is not" ENV_FILE=keyonly.env
```

- [ ] **Step 2: Stubs**

Create `scripts/lib/chain.sh` as a stub that sets every name to `?`, so CI shows each case failing by name (a stub that only failed would pass the cases that expect a refusal, and compare two empty strings as equal):

```bash
#!/usr/bin/env bash
# Stub: test-chain.sh runs against this and fails by name. The next commit fills it in.
chain_select() {
  CH_NAME='?' CH_ENV_FILE='?' CH_PROJECT='?' CH_SVC_TREASURY='?' CH_SVC_SIGNER='?' CH_SVC_ORCH='?'
  CH_TREASURY='?' CH_SIGNER='?' CH_ORCH='?' CH_TREASURY_PG='?' CH_ORCH_PG='?' CH_BACKUP_DIR='?'
}
chain_compose_args() { echo '?'; }
```

(`check-cap-invariants.sh` stays unchanged: it ignores `ENV_FILE`, so the new cases fail on their text.) In `.github/workflows/test-treasury-scripts.yml`: add `- "scripts/lib/chain.sh"`, `- "scripts/test-chain.sh"`, `- "docker-compose.treasury.yml"` and `- "docker-compose.mainnet.treasury.yml"` to both `paths:` lists (the self-check reads both compose files, so a rename in either must run it); add `bash -n scripts/lib/chain.sh` and `bash -n scripts/test-chain.sh` to the `Syntax` step; and after the `Mainnet compose guard` step add:

```yaml
      - name: Chain helper
        if: ${{ !cancelled() }}
        run: bash scripts/test-chain.sh
```

- [ ] **Step 3: Commit the red state**

Commit message file `commit-msg.txt`:

```text
test: the chain helper's names, and ENV_FILE for the invariants check

scripts/lib/chain.sh will say which treasury stack a script acts on,
stage by default. Its self-check pins the stage names the scripts used
to hardcode, the mainnet names the new compose file produces, and ties
both to the compose files. check-cap-invariants.sh will read the file
ENV_FILE names. The helper is a stub, so CI shows each case failing by
name first.

Co-Authored-By: Claude <noreply@anthropic.com>
```

```bash
git add scripts/lib/chain.sh scripts/test-chain.sh scripts/test-cap-invariants.sh .github/workflows/test-treasury-scripts.yml
git commit -F commit-msg.txt
```

Controller: run CI. Expected: "Test treasury scripts" fails; `test-chain.sh` shows `FAIL` by name for 9 of its 11 cases (`2 passed, 9 failed`); the two that pass already read the compose files, not the helper, and are the guards (`every mainnet service is in docker-compose.mainnet.treasury.yml` and `every stage service is in docker-compose.treasury.yml`); `test-cap-invariants.sh` shows `FAIL` by name for `ENV_FILE names another file: its limits are read`, `ENV_FILE wins over .env` and `a key without the network in ENV_FILE is refused`, while `without ENV_FILE the checker still reads .env` passes already (it is the guard); the earlier 17 cases stay `ok`.

- [ ] **Step 4: The helper**

Replace `scripts/lib/chain.sh`:

```bash
#!/usr/bin/env bash
#
# Which treasury stack a script or a workflow acts on: CHAIN=stage (the default; the testnet) or
# CHAIN=mainnet. Source it, then call chain_select:
#
#   . "$(dirname "$0")/lib/chain.sh"
#   chain_select "${CHAIN:-stage}" || exit 1
#
# chain_select sets, and only sets, these variables:
#
#   CH_NAME          stage | mainnet
#   CH_ENV_FILE      the env file that stack reads: .env | .env.mainnet
#   CH_PROJECT       the compose project
#   CH_SVC_TREASURY  the compose service names of the three app services
#   CH_SVC_SIGNER
#   CH_SVC_ORCH
#   CH_TREASURY      the containers: the three app services, then their two databases
#   CH_SIGNER
#   CH_ORCH
#   CH_TREASURY_PG
#   CH_ORCH_PG
#   CH_BACKUP_DIR    where this stack's database dumps are written
#
# The stage names are the ones the operator scripts hardcoded before this file existed. The mainnet
# names are the ones docker-compose.mainnet.treasury.yml produces, and the two stacks never share a
# container name or an app service name (see that file on why). test-chain.sh pins all of it.

chain_select() {
  case "${1:-stage}" in
    stage)
      CH_NAME=stage
      CH_ENV_FILE=.env
      CH_PROJECT=clutch-stage
      CH_SVC_TREASURY=treasury-service
      CH_SVC_SIGNER=tron-signer
      CH_SVC_ORCH=payment-orchestrator
      CH_TREASURY=clutch-stage-treasury-service-1
      CH_SIGNER=clutch-stage-tron-signer-1
      CH_ORCH=clutch-stage-payment-orchestrator-1
      CH_TREASURY_PG=clutch-stage-treasury-postgres-1
      CH_ORCH_PG=clutch-stage-orchestrator-postgres-1
      CH_BACKUP_DIR=backups
      ;;
    mainnet)
      CH_NAME=mainnet
      CH_ENV_FILE=.env.mainnet
      CH_PROJECT=clutch-main-treasury
      CH_SVC_TREASURY=mainnet-treasury-service
      CH_SVC_SIGNER=mainnet-tron-signer
      CH_SVC_ORCH=mainnet-payment-orchestrator
      CH_TREASURY=clutch-main-treasury-mainnet-treasury-service-1
      CH_SIGNER=clutch-main-treasury-mainnet-tron-signer-1
      CH_ORCH=clutch-main-treasury-mainnet-payment-orchestrator-1
      CH_TREASURY_PG=clutch-main-treasury-treasury-postgres-1
      CH_ORCH_PG=clutch-main-treasury-orchestrator-postgres-1
      CH_BACKUP_DIR=backups/mainnet
      ;;
    *)
      echo "ABORT: CHAIN must be stage or mainnet, got '${1}'." >&2
      return 1
      ;;
  esac
}

# The arguments `docker compose` takes for the selected stack, one per line so a path with a space
# cannot split. Stage is the four files its deploy uses; mainnet is its one file and its own env file,
# which is what keeps its secrets apart from the testnet's.
chain_compose_args() {
  if [ "$CH_NAME" = mainnet ]; then
    printf '%s\n' -p "$CH_PROJECT" --env-file "$CH_ENV_FILE" -f docker-compose.mainnet.treasury.yml
  else
    printf '%s\n' -p "$CH_PROJECT" -f docker-compose.yml -f docker-compose.treasury.yml \
      -f docker-compose.stage.cloudflare-flex.yml -f docker-compose.stage.treasury.yml
  fi
}

# chain_compose <docker compose arguments>: docker compose for the selected stack.
chain_compose() {
  local args=() a
  while IFS= read -r a; do args+=("$a"); done < <(chain_compose_args)
  docker compose "${args[@]}" "$@"
}
```

In `scripts/check-cap-invariants.sh`, three edits. After `cd "$(dirname "$0")/.."` add:

```bash
# The file the live values come from: .env (the stage stack) unless ENV_FILE names another, as the
# mainnet treasury's is .env.mainnet. Relative to the repository root, where this script runs.
ENV_FILE="${ENV_FILE:-.env}"
```

in `val()`, replace the two lines

```bash
  if [ -z "$v" ] && [ -f .env ]; then
    v=$(grep -E "^$name=" .env | head -1 | cut -d= -f2- | sed -e 's/^"//' -e 's/"$//' || true)
```

with

```bash
  if [ -z "$v" ] && [ -f "$ENV_FILE" ]; then
    v=$(grep -E "^$name=" "$ENV_FILE" | head -1 | cut -d= -f2- | sed -e 's/^"//' -e 's/"$//' || true)
```

and in the header comment replace `# Reads the live values from .env where set,` with `# Reads the live values from .env (or the file ENV_FILE names) where set,`.

- [ ] **Step 5: Commit the green state**

Commit message file `commit-msg.txt`:

```text
feat: one CHAIN switch for the treasury tools, and ENV_FILE for the invariants

scripts/lib/chain.sh resolves CHAIN=stage (default) or CHAIN=mainnet to
the env file, compose project and files, service names, containers and
backup directory, so the operator scripts stop hardcoding clutch-stage
names. The stage values are the old hardcoded ones. check-cap-invariants.sh
reads the file ENV_FILE names, so the mainnet limits can be checked
where they live.

Co-Authored-By: Claude <noreply@anthropic.com>
```

```bash
git add scripts/lib/chain.sh scripts/check-cap-invariants.sh
git commit -F commit-msg.txt
```

Controller: run CI. Expected: "Test treasury scripts" succeeds; `test-chain.sh` says `ok` by name for all 11 cases (`11 passed, 0 failed`); `test-cap-invariants.sh` says `ok` by name for its 17 earlier cases and the four new ones (`21 passed, 0 failed`); `Check monitoring config` and `Test image pinning` stay green.

---

### Task 3: The operator tools take `CHAIN` (clutch-deploy)

**Files:**
- Create: `scripts/test-chain-names.sh`
- Modify: `scripts/halt-minting.sh`, `scripts/resume-minting.sh`, `scripts/set-mint-caps.sh`, `scripts/activate-float.sh`, `scripts/sweep-address.sh`, `scripts/backup-treasury-db.sh`
- Modify: `.github/workflows/halt-minting.yml`, `resume-minting.yml`, `set-mint-caps.yml`, `activate-float.yml`, `sweep-address.yml`, `backup-treasury-db.yml`
- Modify: `.github/workflows/test-treasury-scripts.yml` (`bash -n`, the self-check, `paths:`)

**Interfaces:**
- Consumes: Task 2's `chain_select`, `chain_compose`, `CH_*`; `ENV_FILE` for `check-cap-invariants.sh`.
- Produces: the six scripts act on the stack `CHAIN` names (stage when unset); five workflows with a `chain` choice (default `stage`) whose mainnet runs need the typed word `<word> mainnet`; a nightly backup that also backs up the mainnet treasury once its containers exist.

Decisions 4 and 8. Left alone on purpose: `fund-float` (the TRX rail's fee account; GasFree needs none), `mint-intent`, `redrive-mint`, `reverse-mint`, `close-repaid-deposit` and the restore rehearsal — the mainnet versions come with the plan that opens mainnet to users.

The scripts only run against live containers, so CI checks their syntax, and `test-chain-names.sh` checks the one thing a syntax check cannot: that no script still names a stage container, that each one takes its names from the helper, and that each workflow offers the choice, demands the longer word for mainnet and passes `CHAIN` to the host.

- [ ] **Step 1: The self-check**

Create `scripts/test-chain-names.sh` (it fails by name against the unchanged scripts, so there is no stub):

```bash
#!/usr/bin/env bash
# Self-check that the operator scripts and workflows are chain-aware. The scripts act on live
# containers, so CI cannot run them; this reads them for what would quietly undo the switch: a stage
# container or project name left in a script, a script that does not use the chain helper, a workflow
# without the choice, without the longer typed word for mainnet, that never tells the host which chain,
# or an activate-float.sh that would print the account xpub into a public log.
set -euo pipefail
cd "$(dirname "$0")/.."

passed=0
failed=0
pass() { passed=$((passed + 1)); echo "ok    $1"; }
fail() { failed=$((failed + 1)); echo "FAIL  $1"; }

for s in halt-minting resume-minting set-mint-caps activate-float sweep-address backup-treasury-db; do
  f="scripts/$s.sh"
  if grep -q 'clutch-stage' "$f"; then fail "$s.sh names no stage container or project"; else pass "$s.sh names no stage container or project"; fi
  if grep -q 'lib/chain.sh' "$f" && grep -q 'chain_select' "$f"; then
    pass "$s.sh takes its names from the chain helper"
  else
    fail "$s.sh takes its names from the chain helper"
  fi
done

# <workflow>:<the word it asks for on stage>
for pair in halt-minting:halt resume-minting:resume set-mint-caps:set activate-float:activate sweep-address:sweep; do
  w="${pair%%:*}" word="${pair##*:}"
  f=".github/workflows/$w.yml"
  if grep -q '^      chain:' "$f" && grep -q '^          - mainnet' "$f"; then
    pass "$w.yml offers the chain choice"
  else
    fail "$w.yml offers the chain choice"
  fi
  if grep -q "want=\"$word mainnet\"" "$f"; then
    pass "$w.yml asks for '$word mainnet' on mainnet"
  else
    fail "$w.yml asks for '$word mainnet' on mainnet"
  fi
  # CHAIN must be written to the file the host sources: a name in the `for v in ... ;` list of the
  # three workflows that already pass inputs, or the printf of the two that did not. The bare word
  # CHAIN is not enough: the confirmation step has it too, and a run that never tells the host acts
  # on stage whatever the maintainer chose.
  if grep -qE "for v in [A-Z_ ]*CHAIN;|printf 'CHAIN=%q" "$f" && grep -q 'set -a' "$f"; then
    pass "$w.yml passes CHAIN to the host"
  else
    fail "$w.yml passes CHAIN to the host"
  fi
done

f=.github/workflows/backup-treasury-db.yml
if grep -q 'chain_select mainnet' "$f" && grep -q 'CHAIN=mainnet bash scripts/backup-treasury-db.sh' "$f"; then
  pass "backup-treasury-db.yml backs up the mainnet treasury once it exists"
else
  fail "backup-treasury-db.yml backs up the mainnet treasury once it exists"
fi

# The signer's /internal/xpub reply holds account_xpub. activate-float.sh prints that reply, and with
# CHAIN=mainnet the mainnet xpub would reach a public log, where anyone can derive every deposit
# address from it. The word is in the script only in the filter that removes it.
if grep -q 'account_xpub' scripts/activate-float.sh; then
  pass "activate-float.sh does not print the account xpub"
else
  fail "activate-float.sh does not print the account xpub"
fi

echo ""
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
```

In `.github/workflows/test-treasury-scripts.yml`: add `- "scripts/test-chain-names.sh"`, the six workflow paths `".github/workflows/halt-minting.yml"`, `".github/workflows/resume-minting.yml"`, `".github/workflows/set-mint-caps.yml"`, `".github/workflows/activate-float.yml"`, `".github/workflows/sweep-address.yml"`, `".github/workflows/backup-treasury-db.yml"`, and the four scripts `"scripts/halt-minting.sh"`, `"scripts/resume-minting.sh"`, `"scripts/set-mint-caps.sh"`, `"scripts/backup-treasury-db.sh"` to both `paths:` lists (`activate-float.sh` and `sweep-address.sh` are already listed, in `paths:` and in `bash -n`); add `bash -n` lines for `scripts/test-chain-names.sh`, `scripts/halt-minting.sh`, `scripts/resume-minting.sh`, `scripts/set-mint-caps.sh` and `scripts/backup-treasury-db.sh` to the `Syntax` step; and after the `Chain helper` step add:

```yaml
      - name: Chain-aware tools
        if: ${{ !cancelled() }}
        run: bash scripts/test-chain-names.sh
```

- [ ] **Step 2: Commit the red state**

Commit message file `commit-msg.txt`:

```text
test: the operator tools must be chain-aware, against the stage-only scripts

test-chain-names.sh reads the six operator scripts and their workflows
for a stage name left behind, a missing chain helper, a missing chain
choice, a missing longer word for mainnet and a CHAIN that never
reaches the host, and checks that activate-float.sh does not print the
account xpub. It fails against the scripts as they are.

Co-Authored-By: Claude <noreply@anthropic.com>
```

```bash
git add scripts/test-chain-names.sh .github/workflows/test-treasury-scripts.yml
git commit -F commit-msg.txt
```

Controller: run CI. Expected: "Test treasury scripts" fails; `test-chain-names.sh` shows `FAIL` by name for all 12 script cases (6 scripts × 2), all 15 workflow cases (5 workflows × 3), the backup case and the xpub case — 29 of 29 (`0 passed, 29 failed`); the other suites stay green.

- [ ] **Step 3: The scripts**

`scripts/halt-minting.sh` — replace the two lines

```bash
PG=clutch-stage-treasury-postgres-1
SVC=clutch-stage-treasury-service-1
```

with:

```bash
. "$(dirname "$0")/lib/chain.sh"
chain_select "${CHAIN:-stage}" || exit 1
PG=$CH_TREASURY_PG
SVC=$CH_TREASURY
echo "treasury: $CH_NAME"
```

`scripts/resume-minting.sh` — replace the same two lines (lines 15-16) with the same five lines.

`scripts/sweep-address.sh` — replace

```bash
PG=clutch-stage-treasury-postgres-1
SIGNER=clutch-stage-tron-signer-1
```

with:

```bash
. "$(dirname "$0")/lib/chain.sh"
chain_select "${CHAIN:-stage}" || exit 1
PG=$CH_TREASURY_PG
SIGNER=$CH_SIGNER
echo "treasury: $CH_NAME"
```

`scripts/activate-float.sh` — replace the three lines near the top

```bash
SIGNER=clutch-stage-tron-signer-1
PG=clutch-stage-treasury-postgres-1
TREASURY=clutch-stage-treasury-service-1
```

with (a `source` line only: sourcing the script for `can_activate` still defines functions and nothing else):

```bash
. "$(dirname "${BASH_SOURCE[0]}")/lib/chain.sh"
```

and make the first statements of `main()` set the names — add right after its `local c run status age reserve liability owed act xfer msg resp` line:

```bash
  chain_select "${CHAIN:-stage}" || exit 1
  SIGNER=$CH_SIGNER
  PG=$CH_TREASURY_PG
  TREASURY=$CH_TREASURY
  echo "treasury: $CH_NAME"
```

The same function prints the signer's whole `/internal/xpub` reply (its first command after `echo "=== the float, from the signer itself ==="`), and that reply holds `account_xpub`. With `CHAIN=mainnet` the mainnet xpub would go into a public log, and anyone could derive every user's deposit address from it. Change the last line of that command, `2>/dev/null | sed 's/,/,\n    /g' | sed 's/^/    /' || echo "    (could not read /internal/xpub)"`, to:

```bash
    2>/dev/null | sed 's/,/,\n    /g' | grep -v '"account_xpub"' | sed 's/^/    /' || echo "    (could not read /internal/xpub)"
```

The float addresses stay in the reply and in the log; the account xpub does not. (`test-chain-names.sh` checks that the word `account_xpub` is in the script.)

`scripts/set-mint-caps.sh` — right after `set -euo pipefail` add:

```bash
. "$(dirname "$0")/lib/chain.sh"
chain_select "${CHAIN:-stage}" || exit 1
E="$CH_ENV_FILE"
echo "treasury: $CH_NAME ($E)"
```

then replace every use of the env file: `if [ ! -f .env ]; then` → `if [ ! -f "$E" ]; then`; `echo "ABORT: no .env here ($(pwd))."` → `echo "ABORT: no $E here ($(pwd))."`; `cp -a .env .env.bak` → `cp -a "$E" "$E.bak"`; `chmod 600 .env.bak` → `chmod 600 "$E.bak"`; `rm -f .env.bak.*` → `rm -f "$E".bak.*`; inside `set_var()` every `.env` → `"$E"` (the `grep -qE`, the `sed -i` and the `>>`); the two `grep -E '^(PER_TX|DAILY)_MINT_CAP_CLT=' .env` → `... "$E"`; `chmod 600 .env` → `chmod 600 "$E"`. Replace the whole `docker compose -p clutch-stage ... up -d --force-recreate --no-deps treasury-service 2>&1 | tail -5` command (the six lines) with:

```bash
chain_compose up -d --force-recreate --no-deps "$CH_SVC_TREASURY" 2>&1 | tail -5
```

replace the two `docker exec clutch-stage-treasury-service-1 printenv ...` with `docker exec "$CH_TREASURY" printenv ...`, replace `echo "=== restarting treasury-service ==="` with `echo "=== restarting $CH_SVC_TREASURY ==="`, replace `echo "ABORT: treasury-service did not come back up."` with `echo "ABORT: $CH_SVC_TREASURY did not come back up."`, and replace `bash scripts/check-cap-invariants.sh` with `ENV_FILE="$E" bash scripts/check-cap-invariants.sh`. The header line `# Set the treasury's mint caps on stage and restart the service so it reads them.` becomes `# Set the treasury's mint caps (CHAIN=stage by default, or CHAIN=mainnet) and restart the service so it reads them.`

`scripts/backup-treasury-db.sh` — after `cd "$(dirname "$0")/.."` add:

```bash
. scripts/lib/chain.sh
chain_select "${CHAIN:-stage}" || exit 1
ENV_FILE="$CH_ENV_FILE"
```

replace `if [ ! -f .env ]; then` with `if [ ! -f "$ENV_FILE" ]; then` and `echo "ABORT: no .env here ($(pwd))."` with `echo "ABORT: no $ENV_FILE here ($(pwd))."`; in `env_get()` replace `grep -E "^$1=" .env 2>/dev/null` with `grep -E "^$1=" "$ENV_FILE" 2>/dev/null`; replace the two `ABORT` messages that say `.env` (`BACKUP_PASSPHRASE is not set in .env.` and `... missing from .env.`) with `$ENV_FILE`; replace `BACKUP_DIR="${BACKUP_DIR:-backups}"` with `BACKUP_DIR="${BACKUP_DIR:-$CH_BACKUP_DIR}"`; replace the two container defaults with `TREASURY_CONTAINER="${TREASURY_CONTAINER:-$CH_TREASURY_PG}"` and `ORCHESTRATOR_CONTAINER="${ORCHESTRATOR_CONTAINER:-$CH_ORCH_PG}"`; and replace `echo "=== treasury backup $STAMP ==="` with `echo "=== treasury backup $STAMP ($CH_NAME) ==="`. The dump names stay `treasury-<stamp>.dump.enc` and `orchestrator-<stamp>.dump.enc`: the two chains differ by directory (`backups` and `backups/mainnet`) and by the remote in each env file, so neither's retention can prune the other's dumps (Fact 10).

- [ ] **Step 4: The workflows**

All five get the same input and the same confirmation step, with the word for that workflow. Under `inputs:` add (before the `confirm:` input):

```yaml
      chain:
        description: "Which treasury"
        type: choice
        default: stage
        options:
          - stage
          - mainnet
```

and change the `confirm` input's description to say the longer word, e.g. for `halt-minting.yml`: `'Type "halt" to confirm stopping new mints (type "halt mainnet" for the mainnet treasury)'`. Replace the whole step named `Check the confirmation` with the following, using that workflow's word (`halt`, `resume`, `set`, `activate` or `sweep`) in both places:

```yaml
      - name: Check the confirmation
        env:
          CONFIRM: ${{ inputs.confirm }}
          CHAIN: ${{ inputs.chain }}
        run: |
          want="halt"
          [ "$CHAIN" = "mainnet" ] && want="halt mainnet"
          if [ "$CONFIRM" != "$want" ]; then
            echo "::error::confirm must be exactly '$want' for $CHAIN — got '$CONFIRM'"
            exit 1
          fi
```

(The line `want="halt"` is `want="resume"`, `want="set"`, `want="activate"` or `want="sweep"` in the others, and the second line is `want="<word> mainnet"` — `test-chain-names.sh` greps for exactly that.) In each workflow's SSH step, add to its `env:`

```yaml
          CHAIN: ${{ inputs.chain }}
```

and make sure the host receives it. `halt-minting.yml`, `sweep-address.yml` and `set-mint-caps.yml` already pass their inputs as a file of shell-quoted assignments: change `for v in HALT_REASON;` to `for v in HALT_REASON CHAIN;`, `for v in ADDRESS;` to `for v in ADDRESS CHAIN;` and `for v in PER_TX DAILY;` to `for v in PER_TX DAILY CHAIN;`. `resume-minting.yml` and `activate-float.yml` pass nothing yet: after their `SSH_OPTS=` line add

```bash
          printf 'CHAIN=%q\n' "$CHAIN" > "$RUNNER_TEMP/clutch-env.sh"
          sshpass -e ssh $SSH_OPTS "$CLUTCH_SSH_USER@$CLUTCH_SSH_HOST" "cat > $R.env" < "$RUNNER_TEMP/clutch-env.sh"
```

and replace the last command's `bash $R.sh` with `bash -c 'set -a; . $R.env; set +a; rm -f $R.env; exec bash $R.sh'`, exactly as `set-gasfree-settings.yml` ends. (Through `env:`, never interpolated into the script body: a typed value must not become part of a command.)

`backup-treasury-db.yml` has no inputs. Replace the one line `bash scripts/backup-treasury-db.sh` in its remote script with:

```bash
          rc=0
          bash scripts/backup-treasury-db.sh || rc=1
          # The mainnet treasury is backed up once it exists, and a mainnet treasury that exists but
          # cannot be backed up fails the run. Before its first start there is nothing to dump.
          . scripts/lib/chain.sh
          chain_select mainnet
          if docker inspect "$CH_TREASURY_PG" >/dev/null 2>&1; then
            CHAIN=mainnet bash scripts/backup-treasury-db.sh || rc=1
          else
            echo "mainnet treasury: no containers yet, nothing to back up"
          fi
          exit "$rc"
```

- [ ] **Step 5: Commit the green state**

Commit message file `commit-msg.txt`:

```text
feat: the operator tools act on the stack CHAIN names

halt-minting, resume-minting, set-mint-caps, activate-float,
sweep-address and backup-treasury-db take their container, project and
env-file names from scripts/lib/chain.sh instead of hardcoding the
stage ones; CHAIN unset means stage, exactly as before. The five
workflows offer a stage/mainnet choice and ask for "<word> mainnet" on
mainnet. The nightly backup also dumps the mainnet databases once their
containers exist, into their own directory, and fails when a mainnet
treasury that exists cannot be backed up.

Co-Authored-By: Claude <noreply@anthropic.com>
```

```bash
git add scripts/halt-minting.sh scripts/resume-minting.sh scripts/set-mint-caps.sh scripts/activate-float.sh scripts/sweep-address.sh scripts/backup-treasury-db.sh .github/workflows/halt-minting.yml .github/workflows/resume-minting.yml .github/workflows/set-mint-caps.yml .github/workflows/activate-float.yml .github/workflows/sweep-address.yml .github/workflows/backup-treasury-db.yml
git commit -F commit-msg.txt
```

Controller: run CI. Expected: "Test treasury scripts" succeeds; `test-chain-names.sh` says `ok` by name for all 29 cases (`29 passed, 0 failed`); `test-activate-float.sh` still says `10 passed, 0 failed` (the sourced script defines its functions as before); `bash -n` passes on every script; the other suites stay green. The task reviewer checks, by reading, that with `CHAIN` unset every script resolves to exactly the names and files it had before (compare each replaced line with Task 2's stage values).

### Task 4: Start the mainnet treasury, after checks (clutch-deploy)

**Files:**
- Create: `scripts/lib/mainnet-preflight.sh`
- Create: `scripts/test-mainnet-preflight.sh`
- Create: `scripts/mainnet-treasury-up.sh`
- Create: `.github/workflows/mainnet-treasury-up.yml`
- Modify: `.github/workflows/test-treasury-scripts.yml` (`bash -n`, the self-check, `paths:`)

**Interfaces:**
- Consumes: Task 1's compose file and names; Task 2's `chain_select`, `chain_compose` and `ENV_FILE`.
- Produces: `preflight <mainnet env file> <stage env file>` (prints `OK`/`FAIL` lines, names only, returns 1 on any `FAIL`); the workflow "Mainnet — start the treasury" (typed `START MAINNET TREASURY`); a running project `clutch-main-treasury`.

Decision 6. The preflight is the only part with logic worth a test, so it is a function in its own file that the start script and the self-check both source. The start script runs only against live docker, so CI checks its syntax and the task reviewer reads it. **No script here prints a value from either env file**: the checks compare values and print the names of the settings that fail. Starting twice is safe: `up -d` recreates only what changed, and nothing here runs `down`.

- [ ] **Step 1: The self-check, against a stub**

Create `scripts/test-mainnet-preflight.sh`:

```bash
#!/usr/bin/env bash
# Self-check for the mainnet preflight: each way a mainnet treasury start should be refused, by exit
# code and by the line printed, and that no secret value is ever printed. CI runs it
# (test-treasury-scripts.yml) with fixture env files in a temp directory: no docker, no host.
set -euo pipefail
cd "$(dirname "$0")/.."

T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT

passed=0
failed=0

# The stage file: only what the separation checks compare.
cat > "$T/stage.env.base" <<'EOF'
MAINNET_JWT_SECRET=jwt-mainnet-x
JWT_SECRET=jwt-stage-y
DEPOSIT_MNEMONIC=stage mnemonic words
DEPOSIT_ACCOUNT_XPUB=xpub6Cstage
CUSTODY_TRON_ADDRESS=TStageCustody
SIGNER_TOKEN=stage-signer
TREASURY_INITIATOR_TOKEN=stage-i
TREASURY_APPROVER_TOKEN=stage-a
TREASURY_READONLY_TOKEN=stage-r
TREASURY_POSTGRES_PASSWORD=stage-pg1
ORCHESTRATOR_POSTGRES_PASSWORD=stage-pg2
EOF

# A complete mainnet file that differs from the stage one everywhere it must.
cat > "$T/mainnet.env.base" <<'EOF'
CUSTODY_TRON_ADDRESS=TMainCustody
TRONGRID_URL=https://api.trongrid.io
USDT_CONTRACT=TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t
AZURE_TENANT_ID=tenant
AZURE_CLIENT_ID=client
AZURE_CLIENT_SECRET=client-secret-value
AZURE_VAULT_URL=https://v.vault.azure.net
AZURE_KEY_NAME=key
AZURE_KEY_VERSION=v1
DEPOSIT_MNEMONIC=mainnet mnemonic words
DEPOSIT_ACCOUNT_XPUB=xpub6Dmain
PAYOUT_FLOAT_ADDRESS=TMainFloat
SIGNER_TOKEN=main-signer
TREASURY_INITIATOR_TOKEN=main-i
TREASURY_APPROVER_TOKEN=main-a
TREASURY_READONLY_TOKEN=main-r
TREASURY_POSTGRES_PASSWORD=main-pg1
ORCHESTRATOR_POSTGRES_PASSWORD=main-pg2
JWT_SECRET=jwt-mainnet-x
BACKUP_PASSPHRASE=main-backup-pass
PER_TX_MINT_CAP_CLT=1000000000
DAILY_MINT_CAP_CLT=2000000000
MAX_REDEMPTION_CLT=200000000
MIN_REDEMPTION_CLT=25000000
PER_TX_PAYOUT_CAP_USDT=200000000
REDEMPTION_FEE_USDT=2000000
DAILY_PAYOUT_CAP_CLT=1000000000
EOF

. scripts/lib/mainnet-preflight.sh

# check <name> <expected exit code> <text the output must contain> <sed script for the mainnet file, or ''> [<sed script for the stage file>]
check() {
  local name="$1" want="$2" text="$3" msed="$4" ssed="${5:-}" out code=0
  cp "$T/mainnet.env.base" "$T/mainnet.env"
  cp "$T/stage.env.base" "$T/stage.env"
  [ -z "$msed" ] || sed -i -e "$msed" "$T/mainnet.env"
  [ -z "$ssed" ] || sed -i -e "$ssed" "$T/stage.env"
  chmod 600 "$T/mainnet.env" "$T/stage.env"
  out=$(preflight "$T/mainnet.env" "$T/stage.env" 2>&1) || code=$?
  if [ "$code" -eq "$want" ] && printf '%s' "$out" | grep -qF -- "$text"; then
    passed=$((passed + 1))
    echo "ok    $name"
  else
    failed=$((failed + 1))
    echo "FAIL  $name: exit $code (wanted $want), wanted the text: $text"
    printf '%s\n' "$out" | sed 's/^/        /'
  fi
}

check "a complete, separate mainnet file passes" 0 "every required setting is set" ''
check "a missing required setting fails" 1 "AZURE_KEY_VERSION is empty or missing" '/^AZURE_KEY_VERSION=/d'
check "an empty required setting fails" 1 "SIGNER_TOKEN is empty or missing" 's/^SIGNER_TOKEN=.*/SIGNER_TOKEN=/'
check "a missing limit fails" 1 "REDEMPTION_FEE_USDT is empty or missing" '/^REDEMPTION_FEE_USDT=/d'
check "a missing backup passphrase fails" 1 "BACKUP_PASSPHRASE is empty or missing" '/^BACKUP_PASSPHRASE=/d'
check "the Nile TronGrid fails" 1 "TRONGRID_URL is not https://api.trongrid.io" 's#^TRONGRID_URL=.*#TRONGRID_URL=https://nile.trongrid.io#'
check "the Nile USDT contract fails" 1 "USDT_CONTRACT is not the mainnet USDT contract" 's/^USDT_CONTRACT=.*/USDT_CONTRACT=TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf/'
check "the stage mnemonic fails" 1 "DEPOSIT_MNEMONIC is the same in both files" 's/^DEPOSIT_MNEMONIC=.*/DEPOSIT_MNEMONIC=stage mnemonic words/'
check "the stage xpub fails" 1 "DEPOSIT_ACCOUNT_XPUB is the same in both files" 's/^DEPOSIT_ACCOUNT_XPUB=.*/DEPOSIT_ACCOUNT_XPUB=xpub6Cstage/'
check "the stage custody address fails" 1 "CUSTODY_TRON_ADDRESS is the same in both files" 's/^CUSTODY_TRON_ADDRESS=.*/CUSTODY_TRON_ADDRESS=TStageCustody/'
check "a stage token fails" 1 "SIGNER_TOKEN is the same in both files" 's/^SIGNER_TOKEN=.*/SIGNER_TOKEN=stage-signer/'
check "a stage database password fails" 1 "TREASURY_POSTGRES_PASSWORD is the same in both files" 's/^TREASURY_POSTGRES_PASSWORD=.*/TREASURY_POSTGRES_PASSWORD=stage-pg1/'
check "a JWT secret that is not the mainnet hub's fails" 1 "JWT_SECRET does not match MAINNET_JWT_SECRET" 's/^JWT_SECRET=.*/JWT_SECRET=something-else/'
check "the stage hub's JWT secret fails" 1 "JWT_SECRET is the same in both files" 's/^JWT_SECRET=.*/JWT_SECRET=jwt-stage-y/' 's/^MAINNET_JWT_SECRET=.*/MAINNET_JWT_SECRET=jwt-stage-y/'
check "a plaintext mint key fails" 1 "a plaintext mint key (MINT_AUTHORITY_SECRET) is in the mainnet file" \
  '$a MINT_AUTHORITY_SECRET=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'
check "the non-hex placeholder is fine" 0 "no plaintext mint key" '$a MINT_AUTHORITY_SECRET=unused-this-chain-signs-with-kms'

# A mainnet file other users can read.
cp "$T/mainnet.env.base" "$T/mainnet.env"; cp "$T/stage.env.base" "$T/stage.env"
chmod 644 "$T/mainnet.env"; chmod 600 "$T/stage.env"
code=0; out=$(preflight "$T/mainnet.env" "$T/stage.env" 2>&1) || code=$?
if [ "$code" -eq 1 ] && printf '%s' "$out" | grep -qF "is readable by other users"; then
  passed=$((passed + 1)); echo "ok    a mainnet file other users can read fails"
else
  failed=$((failed + 1)); echo "FAIL  a mainnet file other users can read fails: exit $code"; printf '%s\n' "$out" | sed 's/^/        /'
fi

# Missing files.
code=0; out=$(preflight "$T/nope.env" "$T/stage.env" 2>&1) || code=$?
if [ "$code" -eq 1 ] && printf '%s' "$out" | grep -qF "nope.env does not exist"; then
  passed=$((passed + 1)); echo "ok    a missing mainnet file fails"
else
  failed=$((failed + 1)); echo "FAIL  a missing mainnet file fails: exit $code"; printf '%s\n' "$out" | sed 's/^/        /'
fi
chmod 600 "$T/mainnet.env"
code=0; out=$(preflight "$T/mainnet.env" "$T/nope-stage.env" 2>&1) || code=$?
if [ "$code" -eq 1 ] && printf '%s' "$out" | grep -qF "nope-stage.env does not exist"; then
  passed=$((passed + 1)); echo "ok    a missing stage file fails"
else
  failed=$((failed + 1)); echo "FAIL  a missing stage file fails: exit $code"; printf '%s\n' "$out" | sed 's/^/        /'
fi

# No value from either file is ever printed, even when the check that fails compares it.
cp "$T/mainnet.env.base" "$T/mainnet.env"; cp "$T/stage.env.base" "$T/stage.env"
sed -i -e 's/^DEPOSIT_MNEMONIC=.*/DEPOSIT_MNEMONIC=stage mnemonic words/' -e 's/^SIGNER_TOKEN=.*/SIGNER_TOKEN=stage-signer/' "$T/mainnet.env"
chmod 600 "$T/mainnet.env" "$T/stage.env"
code=0; out=$(preflight "$T/mainnet.env" "$T/stage.env" 2>&1) || code=$?
leak=""
for secret in "stage mnemonic words" "stage-signer" "main-i" "main-pg1" "jwt-mainnet-x" "client-secret-value" "mainnet mnemonic" "main-backup-pass"; do
  printf '%s' "$out" | grep -qF -- "$secret" && leak="$leak [$secret]"
done
if [ "$code" -eq 1 ] && [ -z "$leak" ]; then
  passed=$((passed + 1)); echo "ok    no value from either file is printed"
else
  failed=$((failed + 1)); echo "FAIL  no value from either file is printed: exit $code, printed:$leak"
fi

echo ""
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
```

Create `scripts/lib/mainnet-preflight.sh` as a stub (it exists, and `preflight` passes without checking anything, so every case that expects a failure fails, and the clean case fails on its text):

```bash
#!/usr/bin/env bash
# Stub: test-mainnet-preflight.sh runs against this and fails by name. The next commit fills it in.
preflight() { return 0; }
```

Add the self-check to CI. In `.github/workflows/test-treasury-scripts.yml`: add `- "scripts/lib/mainnet-preflight.sh"`, `- "scripts/test-mainnet-preflight.sh"`, `- "scripts/mainnet-treasury-up.sh"` and `- ".github/workflows/mainnet-treasury-up.yml"` to both `paths:` lists; add `bash -n scripts/lib/mainnet-preflight.sh`, `bash -n scripts/test-mainnet-preflight.sh` and `bash -n scripts/mainnet-treasury-up.sh` to the `Syntax` step; and after the `Chain-aware tools` step add:

```yaml
      - name: Mainnet preflight
        if: ${{ !cancelled() }}
        run: bash scripts/test-mainnet-preflight.sh
```

Create `scripts/mainnet-treasury-up.sh` as a stub too, so `bash -n` has a file: `#!/usr/bin/env bash` then `exit 0`.

- [ ] **Step 2: Commit the red state**

Commit message file `commit-msg.txt`:

```text
test: the mainnet start's preflight, against a stub

preflight refuses a mainnet treasury start when a required setting or a
limit is empty, TronGrid or the USDT contract is not mainnet's, a
secret equals the stage one, the JWT secrets disagree, a plaintext mint
key is present, or the file can be read by other users. It prints
names, never values. This commit adds its self-check and a stub, so CI
shows each case failing by name first.

Co-Authored-By: Claude <noreply@anthropic.com>
```

```bash
git add scripts/lib/mainnet-preflight.sh scripts/test-mainnet-preflight.sh scripts/mainnet-treasury-up.sh .github/workflows/test-treasury-scripts.yml
git commit -F commit-msg.txt
```

Controller: run CI. Expected: "Test treasury scripts" fails, and the self-check shows `FAIL` by name for all 20 of its cases (`0 passed, 20 failed`): the stub prints nothing and returns 0, so the clean case and `the non-hex placeholder is fine` fail on their text, and every case that expects a refusal fails on its exit code; the other suites stay green.

- [ ] **Step 3: The preflight**

Replace `scripts/lib/mainnet-preflight.sh`:

```bash
#!/usr/bin/env bash
#
# The checks before the mainnet treasury is started: preflight <mainnet env file> <stage env file>.
#
# Every line is OK or FAIL, and names a setting, never a value: the two files hold the deposit
# mnemonic, the database passwords and the tokens, and the log of the workflow that runs this is
# public. A value is compared here and never echoed, even when the comparison is what failed.
#
# What it refuses, and why:
#   - a mainnet file that is missing, readable by other users, or lacks a setting the compose file
#     requires (compose would also refuse most of these, but one setting at a time, after pulling), or
#     lacks BACKUP_PASSPHRASE (the nightly backup aborts without it, and a treasury with no backup
#     must not start);
#   - TronGrid or the USDT contract of the testnet: watching the wrong token on the wrong network
#     credits nothing, and nothing says so;
#   - any secret equal to the stage one. One DEPOSIT_MNEMONIC derives the SAME TRON addresses on Nile
#     and on mainnet, so two orchestrators sharing one would hand an address to two users; the same
#     goes for the tokens and passwords, which exist to keep the two stacks apart;
#   - a JWT_SECRET that is not .env's MAINNET_JWT_SECRET: the mainnet hub signs user tokens with
#     that one, and the orchestrator rejects every request signed by anything else;
#   - a plaintext mint key (64 hex characters) in MINT_AUTHORITY_SECRET: the mint authority is the
#     KMS key, and a plaintext one on the host is exactly what the key ceremony removed. The
#     placeholder in .env.mainnet.example (not hex) is fine.

MAINNET_USDT=TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t

# Every setting the mainnet compose file requires with `:?` or that the money path cannot run without,
# and BACKUP_PASSPHRASE: backup-treasury-db.sh aborts without it, so a treasury that started would have
# no backup the first night.
PF_REQUIRED="CUSTODY_TRON_ADDRESS TRONGRID_URL USDT_CONTRACT AZURE_TENANT_ID AZURE_CLIENT_ID AZURE_CLIENT_SECRET AZURE_VAULT_URL AZURE_KEY_NAME AZURE_KEY_VERSION DEPOSIT_MNEMONIC DEPOSIT_ACCOUNT_XPUB PAYOUT_FLOAT_ADDRESS SIGNER_TOKEN TREASURY_INITIATOR_TOKEN TREASURY_APPROVER_TOKEN TREASURY_READONLY_TOKEN TREASURY_POSTGRES_PASSWORD ORCHESTRATOR_POSTGRES_PASSWORD JWT_SECRET PER_TX_MINT_CAP_CLT DAILY_MINT_CAP_CLT MAX_REDEMPTION_CLT MIN_REDEMPTION_CLT PER_TX_PAYOUT_CAP_USDT REDEMPTION_FEE_USDT DAILY_PAYOUT_CAP_CLT BACKUP_PASSPHRASE"

# Settings that must differ between the two files.
PF_DIFFER="DEPOSIT_MNEMONIC DEPOSIT_ACCOUNT_XPUB CUSTODY_TRON_ADDRESS SIGNER_TOKEN TREASURY_INITIATOR_TOKEN TREASURY_APPROVER_TOKEN TREASURY_READONLY_TOKEN TREASURY_POSTGRES_PASSWORD ORCHESTRATOR_POSTGRES_PASSWORD JWT_SECRET BACKUP_PASSPHRASE BACKUP_REMOTE"

pf_ok()  { printf 'OK    %s\n' "$1"; }
pf_bad() { printf 'FAIL  %s\n' "$1"; PF_FAIL=1; }

# First match wins, `=` split on the first one only, surrounding double quotes and a CR stripped.
# Empty when the name is absent: `|| true`, because a grep that matches nothing must not end a
# script that runs under `set -e`.
pf_get() {  # pf_get <file> <name>
  { grep -E "^$2=" "$1" 2>/dev/null | head -1 | cut -d= -f2- | sed -e 's/^"//' -e 's/"$//' | tr -d '\r'; } || true
}

preflight() {  # preflight <mainnet env file> <stage env file>
  local m="$1" s="$2" n mv sv mode
  PF_FAIL=0

  if [ ! -f "$m" ]; then pf_bad "$m does not exist"; return 1; fi
  if [ ! -f "$s" ]; then pf_bad "$s does not exist"; return 1; fi
  pf_ok "$m and $s exist"

  mode=$(stat -c %a "$m")
  if [ "${mode: -1}" = "0" ]; then
    pf_ok "$m is not readable by other users"
  else
    pf_bad "$m is readable by other users (mode $mode): run chmod 600 $m"
  fi

  local missing=0
  for n in $PF_REQUIRED; do
    if [ -z "$(pf_get "$m" "$n")" ]; then pf_bad "$n is empty or missing in $m"; missing=1; fi
  done
  [ "$missing" -eq 0 ] && pf_ok "every required setting is set"

  [ "$(pf_get "$m" TRONGRID_URL)" = "https://api.trongrid.io" ] \
    && pf_ok "TronGrid is mainnet's" || pf_bad "TRONGRID_URL is not https://api.trongrid.io"
  [ "$(pf_get "$m" USDT_CONTRACT)" = "$MAINNET_USDT" ] \
    && pf_ok "the USDT contract is mainnet's" || pf_bad "USDT_CONTRACT is not the mainnet USDT contract"

  local same=0
  for n in $PF_DIFFER; do
    mv=$(pf_get "$m" "$n")
    sv=$(pf_get "$s" "$n")
    if [ -n "$mv" ] && [ "$mv" = "$sv" ]; then
      pf_bad "$n is the same in both files (never share secrets between .env and .env.mainnet)"
      same=1
    fi
  done
  [ "$same" -eq 0 ] && pf_ok "no secret is shared with the stage file"

  # The stage hub's own JWT secret is JWT_SECRET in .env: the mainnet orchestrator must not trust it.
  mv=$(pf_get "$m" JWT_SECRET)
  sv=$(pf_get "$s" MAINNET_JWT_SECRET)
  if [ -n "$mv" ] && [ "$mv" = "$sv" ]; then
    pf_ok "JWT_SECRET matches MAINNET_JWT_SECRET, the mainnet hub's"
  else
    pf_bad "JWT_SECRET does not match MAINNET_JWT_SECRET in $s (the mainnet hub signs user tokens with that one)"
  fi

  mv=$(pf_get "$m" MINT_AUTHORITY_SECRET)
  if printf '%s' "$mv" | grep -Eq '^(0x)?[0-9a-fA-F]{64}$'; then
    pf_bad "a plaintext mint key (MINT_AUTHORITY_SECRET) is in the mainnet file: the mint authority is the KMS key"
  else
    pf_ok "no plaintext mint key in the mainnet file"
  fi

  return "$PF_FAIL"
}
```

- [ ] **Step 4: The start script**

Replace `scripts/mainnet-treasury-up.sh`:

```bash
#!/usr/bin/env bash
#
# Start, or bring up to date, the MAINNET treasury: docker-compose.mainnet.treasury.yml, compose
# project clutch-main-treasury, env file .env.mainnet. The workflow "Mainnet — start the treasury"
# runs it.
#
#   bash scripts/mainnet-treasury-up.sh
#
# It starts nothing until every check has passed, in this order: the preflight (the env files),
# check-cap-invariants.sh on the mainnet file (the limits and the GasFree settings agree), the mainnet
# chain is up, and the compose file renders (compose names any missing setting). Then it pulls the
# pinned images, starts the five services, and waits for each to be healthy.
#
# Safe to run again: `up -d` recreates only what changed. It never runs `down`, never takes `-v`, and
# has no reset: the treasury's two databases live in volumes of this project, and the chain beside it
# in project clutch-main. NOTHING reaches the result from outside: no port is published, no stage
# network is joined, and /payment/ on the mainnet site still answers 503.

set -euo pipefail

cd "$(dirname "$0")/.."
. scripts/lib/chain.sh
. scripts/lib/mainnet-preflight.sh
chain_select mainnet

echo "=== preflight ($CH_ENV_FILE against .env) ==="
if ! preflight "$CH_ENV_FILE" .env; then
  echo ""
  echo "ABORT: the preflight found problems (above). Nothing was started."
  exit 1
fi

echo ""
echo "=== the limits and the GasFree settings agree ==="
if ! ENV_FILE="$CH_ENV_FILE" bash scripts/check-cap-invariants.sh; then
  echo ""
  echo "ABORT: check-cap-invariants.sh found a broken relationship (above). Nothing was started."
  exit 1
fi

echo ""
echo "=== the mainnet chain is up ==="
if ! docker network inspect clutch-mainnet >/dev/null 2>&1; then
  echo "ABORT: the network clutch-mainnet does not exist. Start the chain first (Mainnet — start the chain)."
  exit 1
fi
names=$(docker ps --format '{{.Names}}')
case "$names" in
  *mainnet-node3*) ;;
  *) echo "ABORT: no mainnet-node3 container is running. The treasury reads the chain through it."; exit 1 ;;
esac
echo "  clutch-mainnet exists, mainnet-node3 is running"

echo ""
echo "=== the compose file renders ==="
chain_compose config -q
echo "  ok"

echo ""
echo "=== pulling the pinned images ==="
chain_compose pull

echo ""
echo "=== starting ==="
chain_compose up -d

echo ""
echo "=== waiting for health ==="
unhealthy=0
for c in "$CH_TREASURY_PG" "$CH_ORCH_PG" "$CH_ORCH" "$CH_TREASURY" "$CH_SIGNER"; do
  status=none
  for _ in $(seq 1 60); do
    status=$(docker inspect -f '{{.State.Health.Status}}' "$c" 2>/dev/null || echo none)
    [ "$status" = healthy ] && break
    sleep 2
  done
  if [ "$status" = healthy ]; then
    echo "  $c healthy"
  else
    echo "  $c is not healthy ($status). Its last log lines:"
    docker logs --tail 30 "$c" 2>&1 | sed 's/^/      /' || true
    unhealthy=1
  fi
done

echo ""
echo "=== containers ==="
docker ps --filter "label=com.docker.compose.project=$CH_PROJECT" --format '  {{.Names}}  {{.Status}}  {{.Image}}'

if [ "$unhealthy" -ne 0 ]; then
  echo ""
  echo "ABORT: not every service is healthy. Nothing was stopped; read the logs above, or run PROBE=mainnet-treasury."
  exit 1
fi

echo ""
echo "started. Nothing reaches it from outside: no port is published, no stage network is joined,"
echo "and /payment/ on the mainnet site still answers 503."
```

Create `.github/workflows/mainnet-treasury-up.yml`:

```yaml
# Start, or bring up to date, the MAINNET treasury: docker-compose.mainnet.treasury.yml, project
# clutch-main-treasury, env file .env.mainnet. The checks come first (scripts/mainnet-treasury-up.sh:
# the preflight, check-cap-invariants.sh on .env.mainnet, the chain is up), and nothing is started
# until they all pass. It never runs `down`, has no reset input, and publishes no port: no user can
# reach what it starts. Typed confirmation, like the chain's own start.

name: Mainnet — start the treasury

on:
  workflow_dispatch:
    inputs:
      confirm:
        description: 'Type "START MAINNET TREASURY" to confirm'
        required: true
        default: ""

concurrency:
  group: mainnet-treasury-up
  cancel-in-progress: false

jobs:
  up:
    runs-on: ubuntu-latest
    steps:
      # Through env, never interpolated into the script body.
      - name: Check the confirmation
        env:
          CONFIRM: ${{ inputs.confirm }}
        run: |
          if [ "$CONFIRM" != "START MAINNET TREASURY" ]; then
            echo "Refusing: confirm was '$CONFIRM', expected 'START MAINNET TREASURY'."
            exit 1
          fi

      - name: Start via SSH
        env:
          SSHPASS: ${{ secrets.STAGE_SSH_PASSWORD }}
          CLUTCH_SSH_HOST: ${{ secrets.STAGE_HOST }}
          CLUTCH_SSH_USER: ${{ secrets.STAGE_USER }}
        run: |
          # The same transport as set-mint-caps.yml: plain ssh, and the script travels as a file.
          set -euo pipefail
          command -v sshpass >/dev/null || { sudo apt-get update -qq && sudo apt-get install -y -qq sshpass; }
          R="/tmp/clutch-ci-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}"
          SSH_OPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=30 -o ServerAliveInterval=30"
          cat > "$RUNNER_TEMP/clutch-remote.sh" <<'CLUTCH_REMOTE_EOF'
          set -euo pipefail
          cd "${{ secrets.STAGE_DEPLOY_PATH }}"
          git pull --ff-only origin main
          bash scripts/mainnet-treasury-up.sh
          CLUTCH_REMOTE_EOF
          sshpass -e ssh $SSH_OPTS "$CLUTCH_SSH_USER@$CLUTCH_SSH_HOST" "cat > $R.sh" < "$RUNNER_TEMP/clutch-remote.sh"
          timeout 15m sshpass -e ssh $SSH_OPTS "$CLUTCH_SSH_USER@$CLUTCH_SSH_HOST" "find /tmp -maxdepth 1 -name 'clutch-ci-*' -mtime +1 -delete 2>/dev/null; bash $R.sh"
```

- [ ] **Step 5: Commit the green state**

Commit message file `commit-msg.txt`:

```text
feat: a typed workflow starts the mainnet treasury, after its checks

"Mainnet — start the treasury" runs scripts/mainnet-treasury-up.sh on the
host: the preflight (names, never values), check-cap-invariants.sh on
.env.mainnet, the mainnet chain is up, the compose file renders, then
the pinned images are pulled, the five services started and waited on.
Nothing is started until every check passes; nothing runs `down`; no
port is published and no stage network is joined.

Co-Authored-By: Claude <noreply@anthropic.com>
```

```bash
git add scripts/lib/mainnet-preflight.sh scripts/mainnet-treasury-up.sh .github/workflows/mainnet-treasury-up.yml
git commit -F commit-msg.txt
```

Controller: run CI. Expected: "Test treasury scripts" succeeds; the preflight self-check says `ok` by name for all 20 cases (`20 passed, 0 failed`), and `bash -n` passes on the three new scripts; the other suites stay green. The task reviewer reads `mainnet-treasury-up.sh` against the live path: the order of the gates, that nothing before `up -d` changes the host, and that no line prints a value.

---

### Task 5: `PROBE=mainnet-treasury` (clutch-deploy)

**Files:**
- Modify: `scripts/inspect-stage.sh` (a new probe block, and the fee check in the mainnet part of `PROBE=gasfree`)
- Modify: `.github/workflows/inspect-stage.yml` (the probe choice)
- Modify: `.github/workflows/test-treasury-scripts.yml` (nothing new: `inspect-stage.sh` is already in `bash -n` and `paths:`)

**Interfaces:**
- Consumes: Task 1's names, Task 2's `chain_select`, Task 4's running project.
- Produces: `PROBE=mainnet-treasury` in "Inspect stage (read-only)".

Decision 7. A new block, not a parametrised copy of `treasury` and `sweeper`: those probes are long, heavily used and hardcode the stage names in about forty places, and a block of its own cannot break them. **The run log of this workflow is public**, so the block prints no secret, no address of a user and no user identifier: settings are shown by name for the non-secret ones and as `<set, N chars>` for secrets, alerts have any TRON address masked as `<address>` and are then cut to 70 characters (a treasury alert can start with a user's deposit address, for example `sweep of <address> (index 7) failed`), and the mint-intent and redemption listings carry amounts, statuses and times only. The block reads; it changes nothing.

There is no self-check: the block runs against live containers and the probes are not tested in CI (`bash -n` runs on the file). Its first real run is the rollout's, and it is built so a failing line prints a clear message instead of ending the probe (the script runs under `set -uo pipefail`, without `-e`).

- [ ] **Step 1: The probe block**

In `scripts/inspect-stage.sh`, immediately **above** the comment that starts `# Always succeed.` (the final `exit 0` must stay the last statement: a block below it prints nothing while the run reports success), add:

```bash
if [ "$PROBE" = "mainnet-treasury" ]; then
  # The MAINNET treasury: docker-compose.mainnet.treasury.yml, project clutch-main-treasury. What runs,
  # what it is set to, what it has done, and whether any service name resolves to two containers.
  # Read-only. The run log is public: nothing secret, no address of a user and no user identifier is
  # printed (see Plan 5, Task 5).
  . scripts/lib/chain.sh
  chain_select mainnet
  tq() {  # tq <title> <sql>: one query against the mainnet treasury's database
    echo "--- $1"
    docker exec "$CH_TREASURY_PG" psql -U treasury -d treasury -c "$2" 2>&1 | sed 's/^/    /'
  }

  echo "=== containers ==="
  docker ps -a --filter "label=com.docker.compose.project=$CH_PROJECT" --format '{{.Names}}  {{.Status}}  {{.Image}}' 2>&1 | sed 's/^/    /'
  if [ -z "$(docker ps -aq --filter "label=com.docker.compose.project=$CH_PROJECT" 2>/dev/null)" ]; then
    echo "    (no container: the mainnet treasury has not been started; run \"Mainnet — start the treasury\")"
  fi

  echo ""
  echo "=== health, from inside the project's network ==="
  for pair in "$CH_SVC_TREASURY:8090" "$CH_SVC_SIGNER:8093" "$CH_SVC_ORCH:8091"; do
    name="${pair%%:*}" port="${pair##*:}"
    code=$(docker run --rm --network "${CH_PROJECT}_treasury-network" curlimages/curl:8.10.1 \
             -s -o /dev/null -w '%{http_code}' -m 5 "http://${name}:${port}/health" 2>/dev/null || true)
    echo "    $name /health -> ${code:-no answer}"
  done

  echo ""
  echo "=== published ports (there must be none) and networks (never a stage network) ==="
  for c in "$CH_TREASURY" "$CH_SIGNER" "$CH_ORCH" "$CH_TREASURY_PG" "$CH_ORCH_PG"; do
    ports=$(docker port "$c" 2>/dev/null | tr '\n' ' ')
    nets=$(docker inspect -f '{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}' "$c" 2>/dev/null)
    echo "    $c: ports ${ports:-none}; networks ${nets:-<not running>}"
  done

  echo ""
  echo "=== does every treasury service name resolve to exactly one address? ==="
  echo "    (a name that answers with two addresses is two containers behind one name)"
  dns_count() {  # dns_count <network> <name>
    docker run --rm --network "$1" busybox:1.36 nslookup "$2" 2>/dev/null \
      | awk '/^Name:/ {f=1} f && /^Address/ {n++} END {print n+0}'
  }
  for pair in "clutch-stage_clutch-network:treasury-service" "clutch-stage_clutch-network:tron-signer" \
              "clutch-stage_clutch-network:payment-orchestrator" "clutch-stage_clutch-network:$CH_SVC_ORCH" \
              "clutch-mainnet:$CH_SVC_TREASURY" "clutch-mainnet:$CH_SVC_SIGNER" "clutch-mainnet:$CH_SVC_ORCH"; do
    net="${pair%%:*}" name="${pair##*:}"
    echo "    $name on $net: $(dns_count "$net" "$name") address(es)"
  done
  echo "    (expected: 1 each, and 0 for $CH_SVC_ORCH on the stage network: it must not be there)"

  echo ""
  echo "=== settings (non-secret) ==="
  for pair in "$CH_TREASURY:APP_CHAIN_ID APP_SIGNER_KIND APP_NODE_WS_URL APP_TRONGRID_URL APP_USDT_CONTRACT APP_PER_TX_MINT_CAP_CLT APP_DAILY_MINT_CAP_CLT APP_DAILY_PAYOUT_CAP_CLT APP_REDEMPTION_FEE_USDT APP_RECONCILIATION_INTERVAL_SECS APP_TRANSFER_RAIL APP_GASFREE_NETWORK APP_GASFREE_ACTIVATE_FEE_MAX_USDT APP_GASFREE_TRANSFER_FEE_MAX_USDT APP_MIN_DEPOSIT_USDT" \
              "$CH_SIGNER:APP_PER_TX_PAYOUT_CAP_USDT APP_PAYOUT_FLOAT_TARGET_USDT APP_TRANSFER_RAIL APP_GASFREE_NETWORK APP_GASFREE_API_URL" \
              "$CH_ORCH:APP_MAX_REDEMPTION_CLT APP_MIN_REDEMPTION_CLT APP_REDEMPTIONS_ENABLED APP_ALLOWED_ORIGINS APP_TRANSFER_RAIL APP_GASFREE_NETWORK APP_MIN_DEPOSIT_USDT"; do
    c="${pair%%:*}"
    echo "--- $c"
    for k in ${pair#*:}; do
      v=$(docker exec "$c" printenv "$k" 2>/dev/null || true)
      if [ -n "$v" ]; then echo "    $k=$v"; else echo "    $k=<empty>"; fi
    done
  done
  echo "--- secrets: presence only"
  for pair in "$CH_TREASURY:APP_AZURE_CLIENT_SECRET APP_APPROVER_TOKEN APP_MINT_AUTHORITY_SECRET" \
              "$CH_SIGNER:APP_DEPOSIT_MNEMONIC APP_SIGNER_TOKEN APP_GASFREE_API_KEY APP_GASFREE_API_SECRET" \
              "$CH_ORCH:APP_JWT_SECRET APP_TREASURY_INITIATOR_TOKEN"; do
    c="${pair%%:*}"
    for k in ${pair#*:}; do
      v=$(docker exec "$c" printenv "$k" 2>/dev/null || true)
      if [ -n "$v" ]; then echo "    $c $k=<set, ${#v} chars>"; else echo "    $c $k=<empty>"; fi
    done
  done
  echo "    (APP_MINT_AUTHORITY_SECRET must be <empty>: the mint authority is the KMS key)"

  echo ""
  echo "=== the treasury's own state ==="
  tq "breaker" "select minting_halted, halt_reason, updated_at from breaker_state;"
  tq "last reconciliation runs" "select status, ledger_liability, custody_reported, run_at from reconciliation_runs order by run_at desc limit 5;"
  tq "open alerts (addresses masked, cut to 70 characters; the log is public)" \
     "select severity, source, left(regexp_replace(message, 'T[1-9A-HJ-NP-Za-km-z]{33}', '<address>', 'g'), 70) as message, created_at from alerts order by created_at desc limit 6;"
  tq "mint intents by status" "select status, count(*), sum(amount_clt) as clt from mint_intents group by status order by status;"
  tq "last mint intents (amounts and times only)" \
     "select left(id::text, 8) as id, status, amount_clt, expected_amount_usdt, swept_at is not null as swept, created_at from mint_intents order by created_at desc limit 8;"
  tq "redemptions not yet paid" \
     "select status, count(*), sum(amount_clt) as clt, min(created_at) as oldest from redemption_intents where status in ('burn_confirmed', 'payout_pending', 'payout_submitted') group by status;"
  tq "last redemptions (amounts and times only)" \
     "select status, amount_clt, payout_amount_usdt, created_at, updated_at from redemption_intents order by created_at desc limit 5;"

  echo ""
  echo "=== the GasFree float ==="
  GFM_F=$(docker exec "$CH_SIGNER" sh -c 'curl -fsS -H "Authorization: Bearer $APP_SIGNER_TOKEN" http://localhost:8093/internal/xpub' 2>/dev/null \
            | sed -n 's/.*"payout_gasfree_address"[ ]*:[ ]*"\([^"]*\)".*/\1/p')
  if [ -z "$GFM_F" ]; then
    echo "    the signer names no GasFree float: GasFree is off in tron-signer, or it is not running"
  else
    GFM_TG=$(sed -n 's/^TRONGRID_URL=//p' .env.mainnet 2>/dev/null | head -1 | tr -d '\r')
    [ -n "$GFM_TG" ] || GFM_TG=https://api.trongrid.io
    GFM_C=$(curl -fsS --max-time 20 -X POST "$GFM_TG/wallet/getcontract" -H 'Content-Type: application/json' \
              -d "{\"value\":\"$GFM_F\",\"visible\":true}" 2>/dev/null || true)
    case "$GFM_C" in
      *'"contract_address"'*) echo "    the GasFree float is activated" ;;
      '{}')                   echo "    the GasFree float is NOT activated: redemptions answer 'not available yet' until activate-float.yml runs (CHAIN=mainnet)" ;;
      *)                      echo "    the GasFree float's activation could not be read" ;;
    esac
  fi
fi
```

- [ ] **Step 2: The fee check in `PROBE=gasfree`**

In the mainnet part of `PROBE=gasfree` (merged as #106), inside the branch that runs when the relay answered `"code":200`, right after the `awk` command that prints the two `activateFee` / `transferFee` lines and before the line `echo "--- the fee table, raw ---"`, add:

```bash
      GFM_ACT=$(sed -n 's/^GASFREE_ACTIVATE_FEE_MAX_USDT=//p' .env.mainnet 2>/dev/null | head -1 | tr -d '\r')
      GFM_XFER=$(sed -n 's/^GASFREE_TRANSFER_FEE_MAX_USDT=//p' .env.mainnet 2>/dev/null | head -1 | tr -d '\r')
      if [ -n "$GFM_ACT" ] && [ -n "$GFM_XFER" ]; then
        echo "--- the live fees against the maxima in .env.mainnet ---"
        printf '%s' "$out" | bash scripts/gasfree-fee-check.sh "$GFM_USDT" "$GFM_ACT" "$GFM_XFER" | sed 's/^/    /'
      fi
```

In `.github/workflows/inspect-stage.yml`, add `- mainnet-treasury` to the `probe` choice's `options:` list, right after `- mainnet`. In `scripts/inspect-stage.sh`, add `|mainnet-treasury` at the end of the probe list in the usage comment on line 5 (`PROBE=nginx|containers|git|treasury|sweeper|chain|metrics|bitcart|energy`).

In the `PROBE=metrics` block of `scripts/inspect-stage.sh`, in the `for q in ...; do` line that lists the queries, add one more expression right after the word `clutch_orchestrator_up`, so the rollout can read whether Prometheus scrapes the mainnet treasury:

```text
'min(up{job=~"mainnet-treasury-service|mainnet-payment-orchestrator"})'
```

(`min`, because the loop prints one value and a plain `up` returns two series. The value is 1 only when both jobs are up.)

- [ ] **Step 3: Commit**

Commit message file `commit-msg.txt`:

```text
feat: PROBE=mainnet-treasury, and the mainnet fee check in PROBE=gasfree

A read-only probe of the mainnet treasury: containers and health,
published ports and networks, whether every treasury service name
resolves to exactly one address, the non-secret settings and the
presence of the secrets, the breaker, the reconciliation runs, the
alerts, the mint intents and redemptions (amounts and times only: the
log is public), and whether the GasFree float is activated.
PROBE=gasfree also compares the live mainnet fees with the maxima in
.env.mainnet when it has them.

Co-Authored-By: Claude <noreply@anthropic.com>
```

```bash
git add scripts/inspect-stage.sh .github/workflows/inspect-stage.yml
git commit -F commit-msg.txt
```

Controller: run CI. Expected: "Test treasury scripts" succeeds (`bash -n scripts/inspect-stage.sh` passes; no new case). The task reviewer reads the new block for one thing above all: that no line can print a secret, a user's address or a user identifier into the public log, including on a failure path; and that the block sits above the final `exit 0`. The first real run is Task 8's.

### Task 6: Monitoring tells the chains apart (clutch-deploy)

**Files:**
- Create: `config/monitoring/prometheus/tests/treasury-chains.test.yml`
- Modify: `config/monitoring/prometheus/prometheus.yml`
- Modify: `config/monitoring/prometheus/rules/treasury.yml`
- Modify: `config/monitoring/alertmanager/alertmanager.yml.tpl`
- Modify: `.github/workflows/check-monitoring-config.yml` (a `promtool test rules` step)

**Interfaces:**
- Consumes: Task 1's service names `mainnet-treasury-service` and `mainnet-payment-orchestrator` (the scrape targets, ports 9101 and 9102 as for stage); Fact 11.
- Produces: the scrape jobs `mainnet-treasury-service` and `mainnet-payment-orchestrator` with `chain: mainnet`; `chain: testnet` on the two stage jobs; the alert `TreasuryWatcherCursorStrandedMainnet`; `TreasuryServiceDown` covering the mainnet jobs once they have run; a `chain:` line in the Telegram text.

Decision 9. Two things go wrong when a second treasury exports the same metric names. A rule that picks series by name alone sees both: `TreasuryWatcherCursorStranded` compares a cursor with the **testnet** head, so a healthy mainnet cursor above the testnet's height would page. And a scrape job added before its service exists is `up == 0` for ever, so `TreasuryServiceDown` would page critical for a mainnet treasury nobody has started. The test file pins both (a `promtool test rules` case each).

- [ ] **Step 1: The rule tests, against the unchanged rules**

Create `config/monitoring/prometheus/tests/treasury-chains.test.yml`:

```yaml
# promtool test rules: the treasury rules and the two chains. Run by check-monitoring-config.yml.
#
#   promtool test rules config/monitoring/prometheus/tests/treasury-chains.test.yml
#
# The stage and the mainnet treasury export the same metric names, so a rule that selects by name
# alone sees both. TreasuryWatcherCursorStranded compares a cursor with the TESTNET head: with the
# mainnet treasury scraped, a healthy mainnet cursor above the testnet's height would fire it.
# TreasuryServiceDown must not page for a mainnet treasury that has never been started (its scrape
# target is `up == 0` for ever), and must page once one that ran stops answering.

rule_files:
  - ../rules/treasury.yml

evaluation_interval: 1m

tests:
  - name: a mainnet cursor above the testnet head, below its own, fires neither cursor rule
    interval: 1m
    input_series:
      - series: 'clutch_treasury_chain_cursor_height{job="mainnet-treasury-service", chain="mainnet"}'
        values: '5000+0x15'
      - series: 'latest_block_index{job="mainnet-node1", chain="mainnet", role="validator"}'
        values: '6000+0x15'
      - series: 'clutch_treasury_chain_cursor_height{job="treasury-service", chain="testnet"}'
        values: '1000+0x15'
      - series: 'latest_block_index{job="node1", chain="testnet", role="validator"}'
        values: '2000+0x15'
    alert_rule_test:
      - eval_time: 10m
        alertname: TreasuryWatcherCursorStranded
        exp_alerts: []
      - eval_time: 10m
        alertname: TreasuryWatcherCursorStrandedMainnet
        exp_alerts: []

  - name: a mainnet cursor above the mainnet head fires the mainnet rule only
    interval: 1m
    input_series:
      - series: 'clutch_treasury_chain_cursor_height{job="mainnet-treasury-service", chain="mainnet"}'
        values: '7000+0x15'
      - series: 'latest_block_index{job="mainnet-node1", chain="mainnet", role="validator"}'
        values: '6000+0x15'
      - series: 'clutch_treasury_chain_cursor_height{job="treasury-service", chain="testnet"}'
        values: '1000+0x15'
      - series: 'latest_block_index{job="node1", chain="testnet", role="validator"}'
        values: '2000+0x15'
    alert_rule_test:
      - eval_time: 10m
        alertname: TreasuryWatcherCursorStrandedMainnet
        exp_alerts:
          - exp_labels:
              severity: critical
              job: mainnet-treasury-service
              chain: mainnet
            exp_annotations:
              summary: "The mainnet deposit watcher's cursor is above the mainnet chain head"
              description: "The mainnet treasury's watcher is crediting nothing and will stay silent until the chain grows past its cursor, then resume having skipped everything below. A mint submitted in that window never becomes confirmed. Almost always a chain reset: the cursor is in Postgres and the chain it counted is gone."
      - eval_time: 10m
        alertname: TreasuryWatcherCursorStranded
        exp_alerts: []

  - name: a mainnet treasury that has never been started does not page
    interval: 1m
    input_series:
      - series: 'up{job="mainnet-treasury-service"}'
        values: '0+0x30'
      - series: 'up{job="mainnet-payment-orchestrator"}'
        values: '0+0x30'
    alert_rule_test:
      - eval_time: 20m
        alertname: TreasuryServiceDown
        exp_alerts: []

  - name: a mainnet treasury that ran and then stopped answering pages
    interval: 1m
    input_series:
      - series: 'up{job="mainnet-treasury-service"}'
        values: '1+0x9 0+0x20'
    alert_rule_test:
      - eval_time: 25m
        alertname: TreasuryServiceDown
        exp_alerts:
          - exp_labels:
              severity: critical
              job: mainnet-treasury-service
            exp_annotations:
              summary: "mainnet-treasury-service is not answering scrapes"

  - name: a stage treasury that stops answering still pages
    interval: 1m
    input_series:
      - series: 'up{job="treasury-service"}'
        values: '0+0x30'
    alert_rule_test:
      - eval_time: 20m
        alertname: TreasuryServiceDown
        exp_alerts:
          - exp_labels:
              severity: critical
              job: treasury-service
            exp_annotations:
              summary: "treasury-service is not answering scrapes"
```

In `.github/workflows/check-monitoring-config.yml`, add this step right after the step named `promtool check config (and every rule it references)`:

```yaml
      - name: promtool test rules (the treasury rules are scoped to their chain)
        run: |
          set -euo pipefail
          docker run --rm \
            -v "$PWD/config/monitoring/prometheus/rules:/etc/prometheus/rules:ro" \
            -v "$PWD/config/monitoring/prometheus/tests:/etc/prometheus/tests:ro" \
            --entrypoint promtool prom/prometheus:v3.1.0 \
            test rules /etc/prometheus/tests/treasury-chains.test.yml
```

- [ ] **Step 2: Commit the red state**

Commit message file `commit-msg.txt`:

```text
test: the treasury rules must tell the stage and mainnet chains apart

promtool test rules cases for the cursor rule (a healthy mainnet cursor
above the testnet head must not fire it, and a stranded mainnet cursor
fires its own twin only) and for TreasuryServiceDown (a mainnet
treasury that never ran must not page; one that ran and stopped must;
a stage one still does). They run against the rules as they are.

Co-Authored-By: Claude <noreply@anthropic.com>
```

```bash
git add config/monitoring/prometheus/tests/treasury-chains.test.yml .github/workflows/check-monitoring-config.yml
git commit -F commit-msg.txt
```

Controller: run CI. Expected: "Check monitoring config" fails at the `promtool test rules` step, and its log names a failure for each of these tests, by `alertname` and time: the first (`TreasuryWatcherCursorStranded` fires on the mainnet series against the testnet head), the second (`TreasuryWatcherCursorStrandedMainnet` does not exist, and the testnet rule fires), the fourth (`TreasuryServiceDown` does not cover the mainnet job). The third and fifth pass already: they are the guards.

- [ ] **Step 3: The jobs, the rules and the message**

In `config/monitoring/prometheus/prometheus.yml`, replace the two jobs

```yaml
  - job_name: "treasury-service"
    scrape_interval: 30s
    static_configs:
      - targets: ["treasury-service:9101"]

  - job_name: "payment-orchestrator"
    scrape_interval: 30s
    static_configs:
      - targets: ["payment-orchestrator:9102"]
```

with:

```yaml
  - job_name: "treasury-service"
    scrape_interval: 30s
    static_configs:
      - targets: ["treasury-service:9101"]
        labels: { chain: testnet }

  - job_name: "payment-orchestrator"
    scrape_interval: 30s
    static_configs:
      - targets: ["payment-orchestrator:9102"]
        labels: { chain: testnet }

  # The MAINNET treasury (docker-compose.mainnet.treasury.yml). Its services have `mainnet-` names
  # because a second `treasury-service` on a network Prometheus shares with the stage one would
  # answer next to it, and this job would then scrape either at random. Same ports, same metric
  # names: the `chain` label and the job name are what tell the two apart. Until the mainnet
  # treasury has been started these targets are `up == 0`, which TreasuryServiceDown ignores.
  - job_name: "mainnet-treasury-service"
    scrape_interval: 30s
    static_configs:
      - targets: ["mainnet-treasury-service:9101"]
        labels: { chain: mainnet }

  - job_name: "mainnet-payment-orchestrator"
    scrape_interval: 30s
    static_configs:
      - targets: ["mainnet-payment-orchestrator:9102"]
        labels: { chain: mainnet }
```

In `config/monitoring/prometheus/rules/treasury.yml`, replace the line `        expr: up{job=~"treasury-service|payment-orchestrator"} == 0` (the `TreasuryServiceDown` rule) with:

```yaml
        # A mainnet job counts only once it has been up: until the mainnet treasury is started its
        # target is `up == 0` for ever, and that is not an outage. After that, a stop pages. The
        # seven days are about what Prometheus keeps (200 h), so a service down for longer stops
        # paging by itself; the stage jobs have no such condition.
        expr: |
          up{job=~"treasury-service|payment-orchestrator"} == 0
          or
          (up{job=~"mainnet-treasury-service|mainnet-payment-orchestrator"} == 0
            and on(job) max_over_time(up{job=~"mainnet-treasury-service|mainnet-payment-orchestrator"}[7d]) == 1)
```

In the same file, in the `TreasuryWatcherCursorStranded` rule, replace the last sentence of its comment (`A mainnet treasury gets its own rule.`) with `The mainnet treasury has its own rule below, TreasuryWatcherCursorStrandedMainnet; this one is scoped to the testnet series so a mainnet cursor, scraped under the same metric name, is never compared with the testnet's head.`, and replace its `expr:` line with:

```yaml
        expr: clutch_treasury_chain_cursor_height{chain="testnet"} > scalar(max(latest_block_index{chain="testnet",role="validator"}))
```

and directly after that rule (its last line is the `description` ending `...the chain it counted is gone.`) add:

```yaml
      # The same failure on the mainnet chain (chain 1000): the cursor lives in the mainnet treasury's
      # Postgres and outlives the chain it counted. Silent for ever without this rule.
      - alert: TreasuryWatcherCursorStrandedMainnet
        expr: clutch_treasury_chain_cursor_height{chain="mainnet"} > scalar(max(latest_block_index{chain="mainnet",role="validator"}))
        for: 2m
        labels:
          severity: critical
        annotations:
          summary: "The mainnet deposit watcher's cursor is above the mainnet chain head"
          description: "The mainnet treasury's watcher is crediting nothing and will stay silent until the chain grows past its cursor, then resume having skipped everything below. A mint submitted in that window never becomes confirmed. Almost always a chain reset: the cursor is in Postgres and the chain it counted is gone."
```

In `config/monitoring/alertmanager/alertmanager.yml.tpl`, in the Telegram `message:` template, replace the two lines

```text
          {{ .Annotations.description }}{{ end }}{{ if .Labels.instance }}
          instance: {{ .Labels.instance }}{{ end }}
```

with:

```text
          {{ .Annotations.description }}{{ end }}{{ if .Labels.instance }}
          instance: {{ .Labels.instance }}{{ end }}{{ if .Labels.chain }}
          chain: {{ .Labels.chain }}{{ end }}
```

- [ ] **Step 4: Commit the green state**

Commit message file `commit-msg.txt`:

```text
feat: monitoring scrapes the mainnet treasury and tells the chains apart

Two scrape jobs for mainnet-treasury-service and
mainnet-payment-orchestrator, labelled chain=mainnet; the stage jobs get
chain=testnet. TreasuryWatcherCursorStranded is scoped to the testnet
series and gets a mainnet twin, so a healthy mainnet cursor above the
testnet's height cannot fire the testnet rule. TreasuryServiceDown
covers the mainnet jobs once they have been up, so a mainnet treasury
nobody has started does not page. The Telegram text names the chain.

Co-Authored-By: Claude <noreply@anthropic.com>
```

```bash
git add config/monitoring/prometheus/prometheus.yml config/monitoring/prometheus/rules/treasury.yml config/monitoring/alertmanager/alertmanager.yml.tpl
git commit -F commit-msg.txt
```

Controller: run CI. Expected: "Check monitoring config" succeeds: `promtool check config` finds the new jobs and rule, and `promtool test rules` reports `SUCCESS` for the five tests. The task reviewer reads the Alertmanager template change character by character (a syntax slip there breaks every alert) and checks the inhibit rule `equal: ['job']` still pairs a mainnet job's alerts only with that job's `TreasuryServiceDown`.

---

### Task 7: The settings writer knows the mainnet values (clutch-deploy)

**Files:**
- Modify: `scripts/set-gasfree-settings.sh`
- Modify: `scripts/test-set-gasfree-settings.sh`
- Modify: `.github/workflows/set-gasfree-settings.yml`
- Modify: `.env.mainnet.example` (the decided limits, and the GasFree values the writer must match)

**Interfaces:**
- Consumes: Task 2's `ENV_FILE` for `check-cap-invariants.sh`; the accepted numbers of the Global Constraints; Fact 14.
- Produces: `NETWORK=mainnet bash scripts/set-gasfree-settings.sh` writing the GasFree block and the decided limits into `.env.mainnet`; the workflow choice `mainnet` (typed word `gasfree mainnet`).

Decision 10. The writer is the one reviewed place where the mainnet values live; its self-check compares every value it writes with `.env.mainnet.example`, so the file a human reads and the script that writes cannot drift apart. It never writes or prints the relay's key or secret. The limits it writes are readiness B4's launch values; a pilot that wants smaller mint caps lowers them afterwards with `Set mint caps` (chain mainnet).

- [ ] **Step 1: The self-check, with the mainnet cases**

Replace `scripts/test-set-gasfree-settings.sh`:

```bash
#!/usr/bin/env bash
# Self-check for set-gasfree-settings.sh, against fixture env files in a temp directory. CI runs it
# (test-treasury-scripts.yml) with no docker and no network: the script restarts nothing, and the
# check-cap-invariants.sh it ends with reads only the env file.
set -euo pipefail
cd "$(dirname "$0")/.."

T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/scripts"
cp scripts/set-gasfree-settings.sh scripts/check-cap-invariants.sh "$T/scripts/"

passed=0
failed=0
out=""
code=0
ENVF=.env

# check <name> <condition...>: the condition is a command; its exit status is the verdict.
check() {
  local name="$1"
  shift
  if "$@"; then
    passed=$((passed + 1))
    echo "ok    $name"
  else
    failed=$((failed + 1))
    echo "FAIL  $name"
    printf '%s\n' "$out" | sed 's/^/        /'
  fi
}

# run <NETWORK>: the script in $T, with nothing from this environment but PATH.
run() {
  code=0
  out=$(env -i PATH="$PATH" NETWORK="$1" bash "$T/scripts/set-gasfree-settings.sh" 2>&1) || code=$?
}

said() { printf '%s' "$out" | grep -qF -- "$1"; }
has_line() { grep -qxF -- "$1" "$T/$ENVF"; }

# Every setting the script writes must carry the example file's value: the commented line for a
# GasFree setting, the active line for a limit. matches_example <env file> <example file> <name>...
matches_example() {
  local envf="$1" ex="$2" name want got
  shift 2
  for name in "$@"; do
    want=$(sed -n "s/^# $name=//p" "$ex" | head -1)
    [ -n "$want" ] || want=$(sed -n "s/^$name=//p" "$ex" | head -1)
    got=$(sed -n "s/^$name=//p" "$T/$envf" | sort -u)
    if [ -z "$want" ] || [ "$got" != "$want" ]; then
      out="$name: wrote '$got', $ex has '$want'"
      return 1
    fi
  done
}

NILE_NAMES="TRANSFER_RAIL GASFREE_NETWORK GASFREE_API_URL GASFREE_SERVICE_PROVIDER GASFREE_ACTIVATE_FEE_MAX_USDT GASFREE_TRANSFER_FEE_MAX_USDT MIN_DEPOSIT_USDT GASFREE_EXPECTED_IMPLEMENTATION GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION PAYOUT_FLOAT_TARGET_USDT"
MAINNET_NAMES="$NILE_NAMES PER_TX_MINT_CAP_CLT DAILY_MINT_CAP_CLT MAX_REDEMPTION_CLT MIN_REDEMPTION_CLT PER_TX_PAYOUT_CAP_USDT REDEMPTION_FEE_USDT DAILY_PAYOUT_CAP_CLT"

# ---- nile (the stage .env), unchanged behaviour -------------------------------------------------

# 1. The stage .env as it was left by hand: the key, the secret and the network, one maximum twice at
#    0, a commented line, an unrelated setting, and no newline at the end of the file.
printf '%s\n' UNRELATED=keep-me GASFREE_API_KEY=key-marker-7f3a GASFREE_API_SECRET=secret-marker-9c1d \
  GASFREE_NETWORK=nile GASFREE_ACTIVATE_FEE_MAX_USDT=0 "# GASFREE_API_URL=" GASFREE_ACTIVATE_FEE_MAX_USDT=0 > "$T/.env"
printf 'LAST_LINE=1' >> "$T/.env"
cp "$T/.env" "$T/before"
run nile
check "the half-set block is completed, and the invariants hold" eval '[ "$code" -eq 0 ] && said "All invariants hold"'
check "every value is .env.example's Nile value" matches_example .env .env.example $NILE_NAMES
check "a setting present twice is replaced in both places" eval '[ "$(grep -cx "GASFREE_ACTIVATE_FEE_MAX_USDT=1500000" "$T/.env")" -eq 2 ]'
check "the key, the secret and the other lines are untouched" eval 'has_line GASFREE_API_KEY=key-marker-7f3a && has_line GASFREE_API_SECRET=secret-marker-9c1d && has_line UNRELATED=keep-me && has_line "# GASFREE_API_URL=" && has_line LAST_LINE=1'
check "the key and the secret are never printed" eval '! said key-marker-7f3a && ! said secret-marker-9c1d'
check "the file as it was is kept in .env.bak" cmp -s "$T/.env.bak" "$T/before"

# 2. Run again on its own result: nothing changes.
cp "$T/.env" "$T/after-first"
run nile
check "a second run changes nothing" eval '[ "$code" -eq 0 ] && cmp -s "$T/.env" "$T/after-first"'

# 3. The key missing, or the secret empty: refused, and the file untouched.
printf '%s\n' GASFREE_API_SECRET=secret-marker-9c1d GASFREE_NETWORK=nile > "$T/.env"
cp "$T/.env" "$T/before"
rm -f "$T/.env.bak"
run nile
check "no GASFREE_API_KEY: refused, nothing written" eval '[ "$code" -eq 1 ] && said "GASFREE_API_KEY is not in .env" && cmp -s "$T/.env" "$T/before" && [ ! -e "$T/.env.bak" ]'
printf '%s\n' GASFREE_API_KEY=key-marker-7f3a GASFREE_API_SECRET= > "$T/.env"
cp "$T/.env" "$T/before"
run nile
check "an empty GASFREE_API_SECRET: refused, nothing written" eval '[ "$code" -eq 1 ] && said "GASFREE_API_SECRET is not in .env" && cmp -s "$T/.env" "$T/before"'

# 4. A network that is neither: refused.
run shasta
check "an unknown network: refused" eval '[ "$code" -eq 1 ] && said "NETWORK must be nile or mainnet"'

# ---- mainnet (.env.mainnet): the GasFree block AND the decided limits ---------------------------

# 5. A .env.mainnet as the maintainer left it: the key pair, stage-like limits (one twice), an
#    unrelated setting, no newline at the end. The stage .env beside it must never change.
rm -f "$T/.env" "$T/.env.bak"
printf '%s\n' STAGE_MARK=untouched > "$T/.env"
cp "$T/.env" "$T/stage-before"
printf '%s\n' UNRELATED=keep-me GASFREE_API_KEY=key-marker-7f3a GASFREE_API_SECRET=secret-marker-9c1d \
  PER_TX_MINT_CAP_CLT=50000000 REDEMPTION_FEE_USDT=1000000 PER_TX_MINT_CAP_CLT=50000000 > "$T/.env.mainnet"
printf 'LAST_LINE=1' >> "$T/.env.mainnet"
cp "$T/.env.mainnet" "$T/mainnet-before"
ENVF=.env.mainnet
run mainnet
check "mainnet: the block and the limits are written, and the invariants hold" eval '[ "$code" -eq 0 ] && said "All invariants hold" && said "the redemption fee covers a GasFree payout"'
check "mainnet: every value is .env.mainnet.example's value" matches_example .env.mainnet .env.mainnet.example $MAINNET_NAMES
check "mainnet: a setting present twice is replaced in both places" eval '[ "$(grep -cx "PER_TX_MINT_CAP_CLT=1000000000" "$T/.env.mainnet")" -eq 2 ]'
check "mainnet: the key, the secret and the other lines are untouched, and never printed" eval 'has_line GASFREE_API_KEY=key-marker-7f3a && has_line GASFREE_API_SECRET=secret-marker-9c1d && has_line UNRELATED=keep-me && has_line LAST_LINE=1 && ! said key-marker-7f3a && ! said secret-marker-9c1d'
check "mainnet: the stage .env is never touched" cmp -s "$T/.env" "$T/stage-before"
check "mainnet: the file as it was is kept in .env.mainnet.bak" cmp -s "$T/.env.mainnet.bak" "$T/mainnet-before"
cp "$T/.env.mainnet" "$T/mainnet-after-first"
run mainnet
check "mainnet: a second run changes nothing" eval '[ "$code" -eq 0 ] && cmp -s "$T/.env.mainnet" "$T/mainnet-after-first"'

# 6. No key in .env.mainnet: refused, and the file untouched.
printf '%s\n' GASFREE_API_SECRET=secret-marker-9c1d > "$T/.env.mainnet"
cp "$T/.env.mainnet" "$T/mainnet-before"
rm -f "$T/.env.mainnet.bak"
run mainnet
check "mainnet: no GASFREE_API_KEY: refused, nothing written" eval '[ "$code" -eq 1 ] && said "GASFREE_API_KEY is not in .env.mainnet" && cmp -s "$T/.env.mainnet" "$T/mainnet-before" && [ ! -e "$T/.env.mainnet.bak" ]'

echo ""
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
```

(18 cases: the nile suite's nine, the unknown-network case that replaces the old `NETWORK=mainnet: refused`, and the eight mainnet cases.)

- [ ] **Step 2: Commit the red state**

The script is unchanged, so no stub is needed: it still refuses mainnet, and the mainnet cases fail by name. The self-check now reads `.env.mainnet.example`, so add `- ".env.mainnet.example"` to both `paths:` lists of `.github/workflows/test-treasury-scripts.yml`: a change to that file must run it.

Commit message file `commit-msg.txt`:

```text
test: the settings writer's mainnet cases, against the nile-only script

The writer will write the GasFree block and readiness B4's limits, with
the $2.00 redemption fee, into .env.mainnet. Its self-check now compares
each value with .env.mainnet.example, and checks the stage .env is
never touched, the key and secret are never printed or written, a
second run changes nothing, and a missing key is refused. The script
still refuses mainnet, so the mainnet cases fail by name.

Co-Authored-By: Claude <noreply@anthropic.com>
```

```bash
git add scripts/test-set-gasfree-settings.sh .github/workflows/test-treasury-scripts.yml
git commit -F commit-msg.txt
```

Controller: run CI. Expected: "Test treasury scripts" fails; `test-set-gasfree-settings.sh` shows `FAIL` by name for 7 of its 18 cases (`11 passed, 7 failed`): `an unknown network: refused` (the old script words its refusal differently) and six mainnet cases: `mainnet: the block and the limits are written, and the invariants hold`, `mainnet: every value is .env.mainnet.example's value`, `mainnet: a setting present twice is replaced in both places`, `mainnet: the file as it was is kept in .env.mainnet.bak`, `mainnet: a second run changes nothing` and `mainnet: no GASFREE_API_KEY: refused, nothing written` (it refuses, but with the old text). The two mainnet cases that pass already are guards, because the old script refuses mainnet before it touches anything: `mainnet: the key, the secret and the other lines are untouched, and never printed` and `mainnet: the stage .env is never touched`. The nine nile cases still pass.

- [ ] **Step 3: The example file**

In `.env.mainnet.example`, replace lines 60-71 (the opening rule line, the comment `# Caps. Env vars, NOT genesis -- ...`, the closing rule line, and the seven limit lines `PER_TX_MINT_CAP_CLT=50000000` through `DAILY_PAYOUT_CAP_CLT=100000000`; Task 1 moved nothing above them) with:

```text
# ---------------------------------------------------------------------------------------------
# Limits. Env vars, NOT genesis: they can be changed later with set-mint-caps.yml (chain mainnet),
# which runs check-cap-invariants.sh afterwards. These are readiness B4's decided mainnet set (decided
# 2026-09-17) with the $2.00 redemption fee the maintainer accepted on 2026-10-02: the relay charges
# 1.50 USDT per transfer, and the fee must cover the transfer maximum (2.00). See mainnet-readiness.md
# B4 for what each one bounds. set-gasfree-settings.yml (network mainnet) writes exactly these.
# A pilot that wants smaller mint caps lowers them afterwards.
# ---------------------------------------------------------------------------------------------
PER_TX_MINT_CAP_CLT=1000000000
DAILY_MINT_CAP_CLT=2000000000
MAX_REDEMPTION_CLT=200000000
PER_TX_PAYOUT_CAP_USDT=200000000
MIN_REDEMPTION_CLT=25000000
REDEMPTION_FEE_USDT=2000000
DAILY_PAYOUT_CAP_CLT=1000000000
```

and fill the blank commented GasFree values (leave the key and the secret blank) so the file carries exactly what the writer writes:

```text
# GASFREE_SERVICE_PROVIDER=TLntW9Z59LYY5KEi9cmwk3PKjQga828ird
# GASFREE_ACTIVATE_FEE_MAX_USDT=2000000
# GASFREE_TRANSFER_FEE_MAX_USDT=2000000
# MIN_DEPOSIT_USDT=5000000
```

and `# PAYOUT_FLOAT_TARGET_USDT=1000000000`. Above the block, add one comment line: `# Live mainnet fees on 2026-10-02: 1.50 USDT to activate and 1.50 per transfer; the maxima leave 33% room.`

- [ ] **Step 4: The script and the workflow**

Replace `scripts/set-gasfree-settings.sh`:

```bash
#!/usr/bin/env bash
#
# Write the GasFree rail's settings into an env file, and for mainnet the decided limits too:
#
#   NETWORK=nile    bash scripts/set-gasfree-settings.sh     # the stage .env: step 2.2 of the Nile rollout
#   NETWORK=mainnet bash scripts/set-gasfree-settings.sh     # .env.mainnet: the mainnet rollout
#
# Only settings that are not secrets. GASFREE_API_KEY and GASFREE_API_SECRET are the relay's
# credentials: this never writes or prints them, and refuses to run until a human has put both in the
# file — the key without the network is the one state check-cap-invariants.sh refuses, and the
# network without the key would leave tron-signer on the TRX rail while the other two are not.
#
# Nile writes the GasFree block with .env.example's values. Mainnet writes the block with the values
# the maintainer accepted on 2026-10-02 (the live relay fees were 1.50 USDT to activate and 1.50 per
# transfer) AND the limits of readiness item B4 with the $2.00 redemption fee. They are written
# together because check-cap-invariants.sh relates them: the fee must cover the relay's transfer
# maximum, and the float target must cover the largest payout plus that fee. test-set-gasfree-
# settings.sh checks every value against .env.example and .env.mainnet.example.
#
# Nothing is restarted. "Deploy stage (VPS)" applies the nile values and "Mainnet — start the
# treasury" the mainnet ones, and each runs check-cap-invariants.sh first, as this does at the end.
# Once GasFree is on, keep these set for as long as any user has a GasFree address or the GasFree
# float holds USDT (docs/ON-CALL.md, "The GasFree rail").

set -euo pipefail
cd "$(dirname "$0")/.."

NETWORK="${NETWORK:?NETWORK must be set (nile or mainnet)}"

case "$NETWORK" in
  nile)
    ENV_FILE=.env
    SETTINGS=(
      TRANSFER_RAIL=gasfree
      GASFREE_NETWORK=nile
      GASFREE_API_URL=https://open-test.gasfree.io/nile
      GASFREE_SERVICE_PROVIDER=TKtWbdzEq5ss9vTS9kwRhBp5mXmBfBns3E
      GASFREE_ACTIVATE_FEE_MAX_USDT=1500000
      GASFREE_TRANSFER_FEE_MAX_USDT=500000
      MIN_DEPOSIT_USDT=1000000
      GASFREE_EXPECTED_IMPLEMENTATION=b8eda40b467b45af107f198e94cc2fa1378adf50
      GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION=2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441
      PAYOUT_FLOAT_TARGET_USDT=30000000
    )
    ;;
  mainnet)
    ENV_FILE=.env.mainnet
    SETTINGS=(
      TRANSFER_RAIL=gasfree
      GASFREE_NETWORK=mainnet
      GASFREE_API_URL=https://open.gasfree.io/tron
      GASFREE_SERVICE_PROVIDER=TLntW9Z59LYY5KEi9cmwk3PKjQga828ird
      GASFREE_ACTIVATE_FEE_MAX_USDT=2000000
      GASFREE_TRANSFER_FEE_MAX_USDT=2000000
      MIN_DEPOSIT_USDT=5000000
      GASFREE_EXPECTED_IMPLEMENTATION=a3b0edffa1b94e93d297dcc9b6860175e9b537ec
      GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION=c8b13e3104f8a2d6e915ac132bdeda7faaf84d7d
      PAYOUT_FLOAT_TARGET_USDT=1000000000
      PER_TX_MINT_CAP_CLT=1000000000
      DAILY_MINT_CAP_CLT=2000000000
      MAX_REDEMPTION_CLT=200000000
      MIN_REDEMPTION_CLT=25000000
      PER_TX_PAYOUT_CAP_USDT=200000000
      REDEMPTION_FEE_USDT=2000000
      DAILY_PAYOUT_CAP_CLT=1000000000
    )
    ;;
  *)
    echo "ABORT: NETWORK must be nile or mainnet, got '$NETWORK'."
    exit 1
    ;;
esac

if [ ! -f "$ENV_FILE" ]; then
  echo "ABORT: no $ENV_FILE here ($(pwd))."
  exit 1
fi

# Presence only: the values are never read into this script.
for k in GASFREE_API_KEY GASFREE_API_SECRET; do
  if ! grep -qE "^$k=.+" "$ENV_FILE"; then
    echo "ABORT: $k is not in $ENV_FILE. Put the relay's key and secret in by hand first, unquoted;"
    echo "  this script never writes them. Nothing was changed."
    exit 1
  fi
done

# One backup, overwritten each run, as set-mint-caps.sh keeps it: every copy holds DEPOSIT_MNEMONIC.
cp -a "$ENV_FILE" "$ENV_FILE.bak"
chmod 600 "$ENV_FILE.bak"
rm -f "$ENV_FILE".bak.*

names=""
for s in "${SETTINGS[@]}"; do names="$names|${s%%=*}"; done
PATTERN="^(${names#|})="

echo "=== before ($ENV_FILE) ==="
grep -E "$PATTERN" "$ENV_FILE" | sed 's/^/    /' || echo "    (none set)"

# A file that does not end in a newline would glue the first appended line onto its last one.
if [ -s "$ENV_FILE" ] && [ -n "$(tail -c1 "$ENV_FILE")" ]; then
  echo >> "$ENV_FILE"
fi

# Replace if present — every copy, so the readers that take the first line and compose agree — and
# append if not. sed -i, as set-mint-caps.sh does: it writes a new file under the same name, which is
# safe because no container mounts the env file; compose reads it when it recreates a service.
for s in "${SETTINGS[@]}"; do
  name="${s%%=*}"
  value="${s#*=}"
  if grep -qE "^$name=" "$ENV_FILE"; then
    sed -i "s#^$name=.*#$name=$value#" "$ENV_FILE"
  else
    printf '%s=%s\n' "$name" "$value" >> "$ENV_FILE"
  fi
done
chmod 600 "$ENV_FILE"

echo ""
echo "=== after ($ENV_FILE) ==="
grep -E "$PATTERN" "$ENV_FILE" | sed 's/^/    /'

echo ""
echo "Nothing was restarted. The next deploy or start applies these, and runs this check first:"
echo ""
ENV_FILE="$ENV_FILE" bash scripts/check-cap-invariants.sh
```

In `.github/workflows/set-gasfree-settings.yml`: add `- mainnet` under the `network` input's `options:` (after `- nile`), change its description to `"Whose values to write: the testnet's (nile, into .env) or mainnet's (into .env.mainnet, with the decided limits)."`, change the `confirm` description to `'Type "gasfree" to confirm writing to the stage host (type "gasfree mainnet" for mainnet)'`, and replace the whole step named `Check the confirmation` with:

```yaml
      - name: Check the confirmation
        env:
          CONFIRM: ${{ inputs.confirm }}
          NETWORK: ${{ inputs.network }}
        run: |
          want="gasfree"
          [ "$NETWORK" = "mainnet" ] && want="gasfree mainnet"
          if [ "$CONFIRM" != "$want" ]; then
            echo "Refusing: confirm was '$CONFIRM', expected '$want' for $NETWORK."
            exit 1
          fi
```

Also change the header comment's last paragraph's "takes no values: the only choice is the network, and only the testnet's exist" to say that the only choice is the network.

- [ ] **Step 5: Commit the green state**

Commit message file `commit-msg.txt`:

```text
feat: the settings writer writes the mainnet GasFree block and limits

NETWORK=mainnet writes .env.mainnet: the GasFree block with the values
the maintainer accepted on 2026-10-02 (maxima 2.00 / 2.00 against live
fees of 1.50 / 1.50, minimum deposit 5.00, float target 1,000, the
relay's provider and both reviewed implementations) and readiness B4's
limits with the $2.00 redemption fee, then runs check-cap-invariants.sh
on the file. The stage .env is never touched, and the key and secret are
never written or printed. The workflow takes the longer word "gasfree
mainnet". .env.mainnet.example carries the same values, and the
self-check compares them.

Co-Authored-By: Claude <noreply@anthropic.com>
```

```bash
git add scripts/set-gasfree-settings.sh .github/workflows/set-gasfree-settings.yml .env.mainnet.example
git commit -F commit-msg.txt
```

Controller: run CI. Expected: "Test treasury scripts" succeeds; `test-set-gasfree-settings.sh` says `ok` by name for all 18 cases (`18 passed, 0 failed`); `Check monitoring config` still succeeds (it renders the compose file from the changed example; the new values are valid). The task reviewer checks that the nile values are byte-identical to before.

After Task 7, **one final whole-branch review** of Tasks 1-7 runs (the most capable model), then the pull request is marked ready.

---

### Task 8: The docs, and the rollout with the maintainer (clutch-deploy, then the host)

**Files:**
- Modify: `docs/ON-CALL.md`, `docs/BACKUP-RESTORE.md`, `docs/ALERTING.md`, `CLAUDE.md` (clutch-deploy)
- Modify (separate pull request): clutch-treasury `docs/mainnet-readiness.md`, after the rollout

**Interfaces:**
- Consumes: everything above.
- Produces: the pages the on-call reader needs, and the mainnet treasury running with GasFree on and no user able to reach it.

- [ ] **Step 1: `docs/ON-CALL.md`**

Add this section directly before `## Things only two people can do`:

```markdown
## The mainnet treasury

A second treasury runs beside the stage one for TRON mainnet: compose project `clutch-main-treasury`, file `docker-compose.mainnet.treasury.yml`, env file `.env.mainnet`. Its three app services are `mainnet-treasury-service`, `mainnet-tron-signer` and `mainnet-payment-orchestrator`, never the stage names: two containers under one name on a network Prometheus or nginx share would send scrapes and requests to either stack. **It is not open to users:** no port is published, it joins no stage network, and `/payment/` on the mainnet site answers 503.

Each tool below takes the same choice as its stage twin. Pick **chain: mainnet** (or **network: mainnet**) and type the longer word: `halt mainnet`, `resume mainnet`, `set mainnet`, `activate mainnet`, `sweep mainnet`, `gasfree mainnet`.

| To do this | Run | Notes |
|---|---|---|
| See everything | `Inspect stage`, probe `mainnet-treasury` | Containers, ports, networks, whether each name resolves to one address, settings, breaker, reconciliation runs, alerts, mint intents, redemptions, the GasFree float. The log is public: it prints no address of a user. |
| Halt minting | `Halt minting`, chain `mainnet` | The same breaker. Rules 1 and 2 at the top of this page apply unchanged. |
| Resume | `Resume minting`, chain `mainnet` | Refuses while the latest reconciliation is a mismatch. |
| Change the mint caps | `Set mint caps`, chain `mainnet` | Runs `check-cap-invariants.sh` on `.env.mainnet` afterwards. |
| Write the GasFree settings and the decided limits | `Set GasFree settings`, network `mainnet` | Needs the relay's key pair in `.env.mainnet`, put there by hand. **Running it again writes the decided limits again**: a pilot's lower mint caps go back to readiness B4's values, so run `Set mint caps` after it. |
| Activate the GasFree float | `Activate the GasFree payout float`, chain `mainnet` | Needs a surplus of at least 4.00 USDT: send real USDT to the custody address first. |
| Sweep one deposit address | `Sweep one deposit address`, chain `mainnet` | Prints the address you give it, and its intent rows, into the public run log. Nothing needs it before the first real deposit. |
| Start it, or bring it up to date | `Mainnet — start the treasury` | Checks first; never runs `down`. |

The alerts are the stage ones, raised from the jobs `mainnet-treasury-service` and `mainnet-payment-orchestrator`, and the Telegram text says `chain: mainnet`. `TreasuryServiceDown` does not page for a mainnet treasury that has never run; once it has, a stop pages. **Never `down -v` against `clutch-main-treasury`:** its two databases are in its volumes.
```

- [ ] **Step 2: `docs/BACKUP-RESTORE.md`**

Add at the end of the file:

```markdown
## Mainnet

The mainnet treasury's two databases are backed up by the same nightly workflow, once their containers exist (`CHAIN=mainnet`). They are written to `backups/mainnet` on the host and copied to the remote named in `.env.mainnet`. **That file has its own `BACKUP_PASSPHRASE`, `BACKUP_REMOTE` and database passwords**: nothing is shared with `.env`, so one leaked secret cannot open both stacks' dumps, and the two retention passes cannot prune each other's files. Set them before the first start. The restore rehearsal (`rehearse-restore.yml`) is stage-only so far: rehearse a mainnet restore before the first real deposit.
```

- [ ] **Step 3: `docs/ALERTING.md` and `CLAUDE.md`**

In `docs/ALERTING.md`: (a) under `## What is still missing`, the `docker stop` / `docker start` lines for `clutch-stage-treasury-service-1` are inside a bash code block (lines 149-152 at `150b0a5`), so add a new paragraph after the end of that code block, not inside it: `For the mainnet treasury the container is clutch-main-treasury-mainnet-treasury-service-1.`; (b) add a row to the rule table at the top of the page, after the `TreasuryWatcherCursorStranded` row: ``| `TreasuryWatcherCursorStrandedMainnet` | the mainnet treasury's watcher cursor sits above the mainnet chain head | critical |``; (c) change `Eleven of them` (line 13) to `Twelve of them` and `those eleven` (line 30) to `those twelve`.

In `CLAUDE.md`, change the words `the mainnet treasury overlay pins its 3 images itself` (in the `Where the pins live` bullet) to `the mainnet treasury file pins its 3 images itself`, and add `mainnet-treasury` after `sweeper` in the list that follows `Probes:`. Then add a section after `## Stage deploy` and its subsections:

```markdown
## The mainnet treasury

`docker-compose.mainnet.treasury.yml` is a complete file (not an overlay on `docker-compose.treasury.yml`) for compose project `clutch-main-treasury`, env file `.env.mainnet`. Its app services have `mainnet-` names (`mainnet-treasury-service`, `mainnet-tron-signer`, `mainnet-payment-orchestrator`) **on purpose**: Compose adds a service's name as an alias on every network it joins, so a second `treasury-service` or `payment-orchestrator` on a network Prometheus or nginx share would answer next to the stage one, and requests, scrapes and the orchestrator's calls to its treasury would reach either stack. `scripts/check-mainnet-compose.sh` fails CI on a shared name, a published port, a stage network, a stage setting missing from the mainnet services, or a stage host in a mainnet URL.

- **Nothing reaches it from outside** until a later plan opens it: no published port, no stage network, `/payment/` answers 503, redemptions are off (`APP_REDEMPTIONS_ENABLED=false`).
- **One switch for the operator tools: `CHAIN=stage|mainnet`** (`scripts/lib/chain.sh`; workflows have a `chain` choice, default stage, and ask for `<word> mainnet` on mainnet). `ENV_FILE` does the same for `check-cap-invariants.sh`. `fund-float`, `mint-intent`, `redrive-mint`, `reverse-mint`, `close-repaid-deposit` and the restore rehearsal are still stage-only.
- **Start it with the workflow "Mainnet — start the treasury"**: `scripts/mainnet-treasury-up.sh` runs the preflight (names, never values: a secret equal to the stage one, Nile TronGrid or USDT, a JWT secret that is not the mainnet hub's, a plaintext mint key), `check-cap-invariants.sh` on `.env.mainnet`, and only then starts it. Never `down -v`.
- **`PROBE=mainnet-treasury`** shows what runs, its ports and networks, whether every service name resolves to one address, its settings (secrets as presence only), the breaker, reconciliation, alerts and redemptions. The run log is public, so it prints nothing about a user.
- `set-gasfree-settings.yml` (network mainnet) writes the GasFree block and the decided limits into `.env.mainnet`; the relay's key pair is put there by hand.
```

- [ ] **Step 4: Commit the docs**

```bash
git add docs/ON-CALL.md docs/BACKUP-RESTORE.md docs/ALERTING.md CLAUDE.md
git commit -F commit-msg.txt
```

with the message file saying: `docs: the mainnet treasury in the on-call, backup and alerting pages, and the repo guide`, a body of two plain sentences, and the `Co-Authored-By` line.

- [ ] **Step 5: The rollout (maintainer, with the controller reading the probes)**

No code. **Every step that changes the host's env files, deploys, starts something or moves money is the maintainer's, or needs the maintainer's yes in chat for that step.** After each step the gate is `PROBE=mainnet-treasury`: the latest reconciliation run is `ok`, the breaker clear, and no new P1 alert. A failed gate stops the rollout. Record each step's result in the ledger.

1. **Prerequisites (maintainer, on the host, never in chat).** In `.env.mainnet` add `BACKUP_PASSPHRASE` (new: `openssl rand -base64 48`, kept somewhere that is not the host) and `BACKUP_REMOTE` (an rclone destination for mainnet dumps, for example the same R2 bucket under `mainnet/`). Both must differ from the values in `.env` (the preflight refuses an equal one: with one remote, both chains would write `treasury-<stamp>.dump.enc` into the same folder, and a restore could take the other chain's dump). Confirm `JWT_SECRET` equals `.env`'s `MAINNET_JWT_SECRET` (the preflight checks it).
2. **Merge the pull request (maintainer).** It runs a normal stage deploy. Controller: the stage gate (`PROBE=sweeper`) is unchanged; `PROBE=metrics` shows Prometheus loaded the new jobs; **no alert fires** (`TreasuryServiceDown` must stay quiet for the mainnet jobs, which are `up == 0` and have never been up).
3. **Write the mainnet settings (maintainer dispatches "Set GasFree settings (stage)", network `mainnet`, confirm `gasfree mainnet`).** Expected in the log: `=== after (.env.mainnet) ===` with the 17 settings, then `All invariants hold`, then `the redemption fee covers a GasFree payout's relay fee` and `the payout float fills far enough for the largest payout`.
4. **Check the pins and the fees (controller).** The mainnet file pins `sha-df5243a`; `PROBE=gasfree`'s mainnet section shows `activateFee 1500000` and `transferFee 1500000`, `the live fees against the maxima in .env.mainnet` both `OK`, and both implementations equal to the reviewed values.
5. **First start (maintainer dispatches "Mainnet — start the treasury", confirm `START MAINNET TREASURY`).** Expected: every `OK` of the preflight, `All invariants hold`, `clutch-mainnet exists`, five lines `... healthy`, then `started. Nothing reaches it from outside`. If the preflight prints `FAIL` lines, fix what each names on the host, and run it again: nothing was started.
6. **Read it (controller): `PROBE=mainnet-treasury`.** Expected: five containers up; `/health` 200 for all three services; `ports none` for all five; the three app services on `clutch-main-treasury_treasury-network` and `clutch-mainnet` only; every name `1 address(es)`, and `mainnet-payment-orchestrator on clutch-stage_clutch-network: 0`; `APP_CHAIN_ID=1000`, `APP_SIGNER_KIND=azure_kms`, `APP_MINT_AUTHORITY_SECRET=<empty>`, the mainnet node and TronGrid, `APP_GASFREE_NETWORK=mainnet`, the maxima `2000000`; the breaker clear; a reconciliation run `ok` (at boot); no alert; no mint intent; `the GasFree float is NOT activated`.
7. **Metrics (controller).** `PROBE=metrics` prints `1` for the new `min(up{...})` line, so both mainnet jobs are up; no alert. From now on the bare `clutch_*` numbers in that probe can come from either chain (its loop prints one value of two series): read stage through `PROBE=treasury` and mainnet through `PROBE=mainnet-treasury`. Scoping those expressions with `chain="testnet"` is left to the next plan.
8. **Prove the halt (maintainer).** Dispatch "Halt minting (stage)" with chain `mainnet`, a reason, confirm `halt mainnet`; the probe shows the breaker halted; **after 5 minutes the Telegram message `TreasuryMintingHalted` arrives and says `chain: mainnet`** (the maintainer confirms it). Then "Resume minting (stage)", chain `mainnet`, confirm `resume mainnet`: `minting is no longer halted`.
9. **Prove the backup (maintainer dispatches "Backup treasury databases (stage)").** The log shows the stage dumps, then `=== treasury backup <stamp> (mainnet) ===`, two encrypted files, `copied both dumps off host`, and the run succeeds.
10. **Stop.** Record the results. The mainnet treasury runs with GasFree on and nobody can reach it. Ask the maintainer the decision of Decision 11: build the KMS payout key (A2) before any real USDT, or run a small pilot with the float capped and the risk recorded.
11. **Readiness document (a separate pull request in clutch-treasury).** `docs/mainnet-readiness.md`: the table row for `docker-compose.mainnet.treasury.yml` (line 376 at `e93bbb2`: "An overlay on the testnet treasury file ... an alias so nginx can tell the two orchestrators apart") says it is a complete file of its own with `mainnet-` service names, no published port and no stage network; B4 "applied to `.env.mainnet` on <date>"; B2 "the relay charges 1.50 USDT per transfer; the redemption fee is 2.00"; D3 "mainnet alerts: jobs and chain label, tested by `promtool test rules`"; G3 "a halt proved on the mainnet treasury on <date>"; B1 and A2 unchanged.

Gate for the whole task: steps 1-9 done, each gate read, and nothing in the logs naming a secret or a user.

---

## After this plan

Not in this plan, each its own plan, in this order:

1. **The KMS payout key (A2).** `tron-signer` has no KMS signing: the float's key is derived from `DEPOSIT_MNEMONIC`, which sits in an env variable on the host. Design and build a KMS-backed signer for the float (the address `payout_gasfree_address` derives from), as the mint authority already is. Readiness marks it a Blocker.
2. **The pilot and go-live.** Open mainnet to the maintainer first: attach the orchestrator to the stage network under a name no stage service has (or a gateway), flip `/payment/` from 503 and its deploy gate, turn redemptions on with a reviewed commit, set pilot caps, seed custody with about 5 real USDT, activate the float (`activate mainnet`, surplus of at least 4.00), one real deposit and one real redemption (readiness B1 and B2), a mainnet restore rehearsal, and the remaining tools made chain-aware (`mint-intent`, `redrive-mint`, `reverse-mint`, `close-repaid-deposit`).
3. **Operators.** A second person who can halt (G3) and the validator set (C2) stay the maintainer's decisions.
