#!/usr/bin/env bash
# Fetches one SWE-bench Lite instance from the HuggingFace datasets-server
# and prepares the workdir for a single-instance E2E smoke sweep:
#   - runs-input/dataset.jsonl  (the JSONL row for the runner)
#   - runs-input/config.toml    (config with workdir pointing at the clone)
#   - runs-input/workdir/       (clone of the instance's repo at base_commit)
#
# Used by .github/workflows/swe-bench-nightly.yml. Designed to be invoked
# from the repository root.
set -euo pipefail

OFFSET="${SWE_BENCH_OFFSET:-0}"
DATASET="princeton-nlp/SWE-bench_Lite"

ROOT="$(pwd)"
INPUT_DIR="${ROOT}/runs-input"
WORKDIR="${INPUT_DIR}/workdir"
mkdir -p "${INPUT_DIR}"
rm -rf "${WORKDIR}"

API_URL="https://datasets-server.huggingface.co/rows?dataset=$(printf '%s' "${DATASET}" | sed 's:/:%2F:g')&config=default&split=test&offset=${OFFSET}&length=1"
echo "Fetching ${API_URL}"

ROW_JSON="$(curl --fail --silent --show-error --retry 4 --retry-delay 3 "${API_URL}")"
ROW="$(printf '%s' "${ROW_JSON}" | jq -c '.rows[0].row')"

INSTANCE_ID="$(printf '%s' "${ROW}" | jq -r '.instance_id')"
REPO="$(printf '%s' "${ROW}"      | jq -r '.repo')"
BASE_COMMIT="$(printf '%s' "${ROW}" | jq -r '.base_commit')"

if [[ -z "${INSTANCE_ID}" || "${INSTANCE_ID}" == "null" ]]; then
  echo "fetch: missing instance_id in API response" >&2
  printf '%s\n' "${ROW_JSON}" >&2
  exit 1
fi

echo "Selected instance: ${INSTANCE_ID}"
echo "Repo:              ${REPO}"
echo "Base commit:       ${BASE_COMMIT}"

# Persist the JSONL row exactly as the harness expects (one record per line).
printf '%s\n' "${ROW}" > "${INPUT_DIR}/dataset.jsonl"

# Clone with a blob-less partial clone to avoid pulling full history blobs;
# the checkout will hydrate the blobs we need.
git clone --filter=blob:none --no-checkout "https://github.com/${REPO}.git" "${WORKDIR}"
git -C "${WORKDIR}" fetch --filter=blob:none --depth=1 origin "${BASE_COMMIT}"
git -C "${WORKDIR}" checkout --detach "${BASE_COMMIT}"

# Config overlay: point environment.workdir at the freshly-prepared clone.
# The smoke run does not need the model section; the CLI's --model wins.
cat > "${INPUT_DIR}/config.toml" <<EOF
[environment]
kind = "local"
timeout_secs = 60
workdir = "${WORKDIR}"
EOF

# Emit GH Actions outputs so the workflow can label artifacts / messages.
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  {
    echo "instance_id=${INSTANCE_ID}"
    echo "repo=${REPO}"
    echo "base_commit=${BASE_COMMIT}"
  } >> "${GITHUB_OUTPUT}"
fi

echo "Prepared inputs in ${INPUT_DIR}"
