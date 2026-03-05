#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -lt 1 ]; then
  echo "Usage: $0 <ticket-id> [ticket-id...]" >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo is required" >&2
  exit 1
fi

EVIDENCE_DIR="${CHIEF_EVIDENCE_DIR:-.chief/evidence}"
RUN_ID="${CHIEF_EVIDENCE_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
TSV_FILE="${EVIDENCE_DIR}/${RUN_ID}-cargo-test-runs.tsv"

mkdir -p "${EVIDENCE_DIR}"
printf "ticket_id\tstarted_at_utc\tfinished_at_utc\tresult\tsummary\n" >"${TSV_FILE}"

for ticket_id in "$@"; do
  started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  log_file="${EVIDENCE_DIR}/${RUN_ID}-${ticket_id}.cargo-test.log"

  echo "[${started_at}] running cargo test for ${ticket_id}"
  if cargo test >"${log_file}" 2>&1; then
    run_result="pass"
  else
    run_result="fail"
  fi

  finished_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  if command -v rg >/dev/null 2>&1; then
    summary="$(rg -n 'test result:|Doc-tests|running [0-9]+ tests|FAILED|error:' "${log_file}" | tail -n 20 | tr '\n' ';')"
  else
    summary="$(grep -nE 'test result:|Doc-tests|running [0-9]+ tests|FAILED|error:' "${log_file}" | tail -n 20 | tr '\n' ';')"
  fi

  printf "%s\t%s\t%s\t%s\t%s\n" "${ticket_id}" "${started_at}" "${finished_at}" "${run_result}" "${summary}" >>"${TSV_FILE}"

  if [ "${run_result}" != "pass" ]; then
    echo "[${finished_at}] cargo test failed for ${ticket_id}; see ${log_file}" >&2
    exit 1
  fi

  echo "[${finished_at}] cargo test passed for ${ticket_id}"
done

echo "wrote evidence:"
echo "  ${TSV_FILE}"
echo "  ${EVIDENCE_DIR}/${RUN_ID}-<ticket-id>.cargo-test.log"
