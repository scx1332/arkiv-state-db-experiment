# Security checks

Free, deterministic checks with pinned tools, ported from
[golemfactory/golem-p2p](https://github.com/golemfactory/golem-p2p)
(`harness/security-*`, `.github/workflows/security.yml`). No secrets or API
keys are involved. `scripts/security/tools.sh` installs each tool as a release
binary checked against a pinned SHA-256, so CI and a local run use the same
versions.

## What runs where

| Check | Workflow | When | Fails on |
| --- | --- | --- | --- |
| `cargo deny`: RustSec advisories (vulnerable, unmaintained, unsound, yanked), licenses, bans, sources | `security.yml` | push to main, PRs, Mondays | any finding not ignored in `deny.toml` |
| `cargo audit`: RustSec, every `Cargo.lock` entry | `security.yml` | same | any vulnerability not ignored in `.cargo/audit.toml` |
| `zizmor` (pedantic) and `actionlint` with shellcheck: workflow security and syntax | `security.yml` | same | any finding |
| `gitleaks`: every commit of every branch, plus a rule for key, token and `.env` files | `security.yml` | same | any finding not in `.gitleaksignore` |
| Trivy: OS packages of the image being built | `image.yml` | every build, before publishing | fixable HIGH or CRITICAL |
| Trivy: the published `ghcr.io/scx1332/arkiv-node:main` | `security.yml` | Mondays, manual | fixable HIGH or CRITICAL |

- `image.yml` also uploads Trivy's full report (all severities, SARIF) to the
  repository's code scanning, under Security.
- Trivy cannot read the Rust crates inside a plain binary. `cargo deny` and
  `cargo audit` cover them from `Cargo.lock`.
- Unfixed Debian vulnerabilities are not gating. On 2026-11-22 the
  `bookworm-slim` base had about 105 HIGH and 1 CRITICAL without a Debian fix.

## Exceptions

Each exception names the advisory or finding and gives its reason and date.

- **`deny.toml`:**
  - `RUSTSEC-2026-0387`, crypto-bigint 0.5. It comes in via k256 0.13 through
    alloy, and no 0.5 release fixes it. Remove the entry when alloy moves to
    k256 0.14.
  - `RUSTSEC-2024-0436`, paste: unmaintained, used as a compile-time macro.
  - CDLA-Permissive-2.0 for `webpki-roots` (Mozilla's root certificates, as data).
- **`.cargo/audit.toml`:**
  - the same crypto-bigint entry;
  - `RUSTSEC-2026-0009`, `time`. It is an optional dependency of ruint that
    nothing here builds.
- **`.gitleaksignore`:** seven false positives, all entity keys (hashes) of
  test fixtures and one test bearer string.
- **`.gitleaks.toml`:** allows the exact value `signed-proposer-v1`, the
  authentication scheme name that the status JSON reports.

Add an exception only for a false positive, or for a finding with no fix,
with its reason. Otherwise fix the finding, for example with
`cargo update -p CRATE`.

## Running locally

```sh
scripts/security/tools.sh /tmp/sec cargo-deny cargo-audit zizmor actionlint shellcheck gitleaks trivy
export PATH=/tmp/sec:$PATH
cargo deny --locked check --hide-inclusion-graph
cargo audit
zizmor --persona pedantic .github/workflows   # GH_TOKEN for the online audits
actionlint
gitleaks git --config .gitleaks.toml --log-opts=--all --redact .
trivy image --scanners vuln --severity HIGH,CRITICAL --ignore-unfixed IMAGE
```

## Updating a tool

Change its version and SHA-256 in `scripts/security/tools.sh` together. Take
the digest from the release asset's `digest` in the GitHub API, not from a
hash of an unverified download.
