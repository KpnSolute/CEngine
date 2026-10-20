#!/usr/bin/env bash
# Reports the migration audit matrix as a job summary and fails while any required row is red.
#
# The matrix lives in docs/MIGRATION_AUDIT_MATRIX.md. This script does not decide status;
# it counts it, so the auditor and the builder see the same number on every run.
#
#   rows    = lines starting with "| <ID> |" (M1, P1, R1, S1, H1, ...)
#   red     = row contains "**red**" or "| red"
#   partial = row contains "partial"
#   green   = row contains "**green**" or "| green"
#
# Exit 1 when red > 0. The gate goes green only when the matrix says so, which the
# auditor re-scores by hand from executable evidence.

set -uo pipefail
root=${ROOT:-$(pwd)}
matrix="$root/docs/MIGRATION_AUDIT_MATRIX.md"
summary=${GITHUB_STEP_SUMMARY:-/dev/null}

if [ ! -f "$matrix" ]; then
  echo "matrix not found: $matrix" | tee -a "$summary"
  echo "migration-audit-report: FAIL (no matrix)"
  exit 1
fi

rows=$(grep -cE '^\| [A-Z][0-9]+ ' "$matrix" || true)
red=$(grep -E '^\| [A-Z][0-9]+ ' "$matrix" | grep -cE '\*\*red\*\*|\| red' || true)
partial=$(grep -E '^\| [A-Z][0-9]+ ' "$matrix" | grep -c 'partial' || true)
green=$(grep -E '^\| [A-Z][0-9]+ ' "$matrix" | grep -cE '\*\*green\*\*\|\| green|green \(branch\)' || true)
verdict=$(grep -m1 -iE '^Verdict|Verdict \|' "$matrix" | sed 's/^/  /')

{
  echo "## Migration audit matrix"
  echo
  echo "| metric | count |"
  echo "|---|---|"
  echo "| requirement rows | $rows |"
  echo "| green | $green |"
  echo "| partial | $partial |"
  echo "| red | $red |"
  echo
  echo "$verdict"
  echo
  echo "### Red rows"
  echo
  grep -E '^\| [A-Z][0-9]+ ' "$matrix" | grep -E '\*\*red\*\*|\| red' | awk -F'|' '{gsub(/^ +| +$/,"",$2); gsub(/^ +| +$/,"",$3); print "- **" $2 "** " $3}'
  echo
  echo "Re-scoring rule: a row is green only when its test exists on the audited branch, the test"
  echo "fails when the feature is removed, the gate ran on that branch with a recorded link, and"
  echo "the data is synthetic or sanitised."
} >> "$summary"

echo "migration-audit-report: rows=$rows green=$green partial=$partial red=$red"
[ "$red" -eq 0 ] && { echo "migration-audit-report: PASS"; exit 0; }
echo "migration-audit-report: FAIL ($red red rows)"
exit 1
