#!/usr/bin/env bash
# Installs the security tools of .github/workflows/security.yml and the image
# scan in image.yml (ported from golemfactory/golem-p2p harness/): pinned
# release binaries for Linux x86_64, each checked against a pinned SHA-256
# before it is unpacked. The same versions run in CI and locally.
#
# Usage: scripts/security/tools.sh DIR TOOL...
# Tools: cargo-deny cargo-audit zizmor actionlint shellcheck gitleaks trivy
#        osv-scanner
#
# To update a tool, change its version and digest together. The digest is
# the `digest` of the release asset in the GitHub API (or the project's own
# checksum file), not a hash computed from an unverified download.
set -euo pipefail

if [ $# -lt 2 ]; then
  echo "usage: $0 DIR TOOL..." >&2
  exit 2
fi
if [ "$(uname -sm)" != "Linux x86_64" ]; then
  echo "only Linux x86_64 binaries are pinned" >&2
  exit 2
fi
dir=$1
shift
mkdir -p "$dir"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# fetch NAME VERSION URL SHA256 MEMBER: downloads URL, checks it, and installs
# MEMBER of the archive as DIR/NAME.
fetch() {
  local name=$1 version=$2 url=$3 sha256=$4 member=$5
  curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$tmp/$name.tgz" "$url"
  echo "$sha256  $tmp/$name.tgz" | sha256sum --check --quiet --strict
  tar -xzf "$tmp/$name.tgz" -C "$tmp" "$member"
  install -m 0755 "$tmp/$member" "$dir/$name"
  echo "$name $version"
}

# fetch_bin NAME VERSION URL SHA256: the same for a bare binary.
fetch_bin() {
  local name=$1 version=$2 url=$3 sha256=$4
  curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$tmp/$name" "$url"
  echo "$sha256  $tmp/$name" | sha256sum --check --quiet --strict
  install -m 0755 "$tmp/$name" "$dir/$name"
  echo "$name $version"
}

gh=https://github.com
for tool; do
  case $tool in
    cargo-deny)
      v=0.20.2
      fetch cargo-deny "$v" \
        "$gh/EmbarkStudios/cargo-deny/releases/download/$v/cargo-deny-$v-x86_64-unknown-linux-musl.tar.gz" \
        9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f \
        "cargo-deny-$v-x86_64-unknown-linux-musl/cargo-deny"
      ;;
    cargo-audit)
      v=0.23.0
      fetch cargo-audit "$v" \
        "$gh/rustsec/rustsec/releases/download/cargo-audit/v$v/cargo-audit-x86_64-unknown-linux-musl-v$v.tgz" \
        4f01c74ed84b2dffd0b1d37012a62cdf2a92c96e25bdf8eec7fcc0d4d3eb7811 \
        "cargo-audit-x86_64-unknown-linux-musl-v$v/cargo-audit"
      ;;
    zizmor)
      v=1.32.0
      fetch zizmor "$v" \
        "$gh/zizmorcore/zizmor/releases/download/v$v/zizmor-x86_64-unknown-linux-gnu.tar.gz" \
        bb110cc765eca9cd7efb2e284a956be5974c60027b0970f16397d86208a9957b \
        zizmor
      ;;
    actionlint)
      v=1.7.12
      fetch actionlint "$v" \
        "$gh/rhysd/actionlint/releases/download/v$v/actionlint_${v}_linux_amd64.tar.gz" \
        8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8 \
        actionlint
      ;;
    shellcheck)
      # Used by actionlint for run: scripts when it is on PATH.
      v=0.11.0
      fetch shellcheck "$v" \
        "$gh/koalaman/shellcheck/releases/download/v$v/shellcheck-v$v.linux.x86_64.tar.gz" \
        b7af85e41cc99489dcc21d66c6d5f3685138f06d34651e6d34b42ec6d54fe6f6 \
        "shellcheck-v$v/shellcheck"
      ;;
    gitleaks)
      v=8.30.1
      fetch gitleaks "$v" \
        "$gh/gitleaks/gitleaks/releases/download/v$v/gitleaks_${v}_linux_x64.tar.gz" \
        551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb \
        gitleaks
      ;;
    trivy)
      v=0.76.2
      fetch trivy "$v" \
        "$gh/aquasecurity/trivy/releases/download/v$v/trivy_${v}_Linux-64bit.tar.gz" \
        bc0fc726e017b8927a62b2d3531d7a84ee01769daf5c99ed866dced54089fdbf \
        trivy
      ;;
    osv-scanner)
      v=2.7.1
      fetch_bin osv-scanner "$v" \
        "$gh/google/osv-scanner/releases/download/v$v/osv-scanner_linux_amd64" \
        fb7dab19a8bd9bbe5c3189f09d4b7ee1820c9553a23bac202648a56efd2f6ee8
      ;;
    *)
      echo "unknown tool: $tool" >&2
      exit 2
      ;;
  esac
done
