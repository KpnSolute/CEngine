#!/usr/bin/env bash
# Static migration checks, run by .github/workflows/migration-ci.yml.
#
# Enforces rules that are cheap to state and expensive to discover late:
#   1. no destructive DDL in migration sources this window;
#   2. every migration file is version-prefixed NNNN_name.sql with no gaps within a directory;
#   3. no customer payloads or secrets in committed migration artifacts;
#   4. the ETL extractor refuses to default to a production target;
#   5. every legacy source table named in MIGRATION_MAP.md carries a disposition.
#
# Exit non-zero on the first violated rule. Prints a PASS/FAIL line per rule.
# It is deliberately permissive about layout: the builder owns the final naming,
# and this script is tightened only when a real layout is in the tree.

set -uo pipefail

fail=0
root=${ROOT:-$(pwd)}
printf 'migration-static-check  root=%s\n' "$root"

say() { printf '  %-6s %s\n' "$1" "$2"; }
bad() { say FAIL "$1"; fail=1; }
ok()  { say PASS "$1"; }

# --- 1. destructive DDL -------------------------------------------------------
mapfile -t mig_files < <(find "$root" -path "$root/target" -prune -o -type f \
  \( -path '*/migrations/*.sql' -o -name 'migration_*.sql' \) -print 2>/dev/null)
if [ ${#mig_files[@]} -eq 0 ]; then
  bad "rule 1: no migration sources found (nothing to scan) -- expected at least one *_ce_migration source from the builder"
else
  if grep -nEi '\b(drop\s+(table|column|schema)|alter\s+table\s+[^;]*\bdrop\b|truncate)\b' "${mig_files[@]}" 2>/dev/null; then
    bad "rule 1: destructive DDL present in migration sources"
  else
    ok "rule 1: no destructive DDL in ${#mig_files[@]} migration source(s)"
  fi
fi

# --- 2. ordering and file naming ---------------------------------------------
if [ ${#mig_files[@]} -eq 0 ]; then
  bad "rule 2: cannot verify order without migration files"
else
  broken=0
  while IFS= read -r dir; do
    nums=$(find "$dir" -maxdepth 1 -type f -name '[0-9][0-9][0-9][0-9]_*.sql' \
      -printf '%f\n' | sort | cut -c1-4 | grep -n '' | awk -F: '{ if ($2 != sprintf("%04d", $1)) print $1, $2 }')
    if [ -n "$nums" ]; then bad "rule 2: gap or misnumbering in $dir: $nums"; broken=1; fi
    badname=$(find "$dir" -maxdepth 1 -type f -name '*.sql' ! -name '[0-9][0-9][0-9][0-9]_*.sql')
    if [ -n "$badname" ]; then bad "rule 2: unprefixed migration file(s): $badname"; broken=1; fi
  done < <(find "$root" -path "$root/target" -prune -o -type d -name migrations -print 2>/dev/null)
  [ $broken -eq 0 ] && ok "rule 2: migrations are NNNN_name.sql with no gaps"
fi

# --- 3. secrets / payloads in artifacts --------------------------------------
scan_dirs=()
for d in "$root/docs" "$root/spec" "$root/tests/migration" "$root/tests/fixtures"; do
  [ -d "$d" ] && scan_dirs+=("$d")
done
if [ ${#scan_dirs[@]} -eq 0 ]; then
  bad "rule 3: no docs/spec/tests fixtures directory to scan"
else
  hits=$(grep -rInE '(AIza[0-9A-Za-z_-]{30,}|sk-[A-Za-z0-9]{20,}|sbp_[A-Za-z0-9]{20,}|eyJ[a-zA-Z0-9_-]{20,}\.[a-zA-Z0-9_-]{20,}|postgres(ql)?://[^:]+:[^@[:space:]]+@|SERVICE_ROLE_KEY)' \
    "${scan_dirs[@]}" 2>/dev/null | grep -vE '://[^:]+:\*\*\*@|://[^:]+:__PASSWORD__@|://[^:]+:password@|://ce:ce@' || true)
  if [ -n "$hits" ]; then
    printf '%s\n' "$hits" | head -20
    bad "rule 3: possible secret or credential in committed artifacts"
  else
    ok "rule 3: no keys or credential-shaped strings in committed artifacts"
  fi
fi

# --- 4. extractor refuses production default ---------------------------------
etl=$(find "$root" -path "$root/target" -prune -o -type f \
  \( -name 'migrate_v1.py' -o -name '*extract*.py' \) -print 2>/dev/null | head -1)
if [ -z "$etl" ]; then
  bad "rule 4: no extractor found (expected MJCC-workspace/tools/migrate_v1.py)"
else
  if grep -qE 'REHEARSAL|allow_production|PRODUCTION_TARGET|--target' "$etl" &&
     grep -qE 'refuse|refused|sys\.exit|raise SystemExit|error\(' "$etl"; then
    ok "rule 4: extractor declares a target and refuses an implicit one"
  else
    bad "rule 4: extractor has no explicit-target refusal"
  fi
fi

# --- 5. every legacy table has a disposition ---------------------------------
mapfile -t legacy_tables < <(printf '%s\n' \
  inventory_items inventory_transactions monthly_inventory invoices invoice_items \
  price_review_queue sku_review_queue inventory_categories vendors user_profiles \
  role_permissions events menu_items menu_cycle_days menu_cycle_slots meal_periods \
  haccp_logs daily_operations_logs opening_checklist_items incident_logs \
  flow_assignments servsafe_certifications budget_line_items budget_line_actuals \
  snack_bar_products snack_bar_sales snack_bar_transactions snack_bar_transaction_items \
  archived_files audit_events commits commit_changes pull_requests staging_entries \
  import_batches ai_providers ai_provider_keys ai_usage_logs agent_threads agent_conversations \
  week_status month_status month_periods monthly_snapshots item_barcodes centers \
  workspace_projects workspace_sites tenants)
map="$root/docs/MIGRATION_MAP.md"
if [ ! -f "$map" ]; then
  bad "rule 5: docs/MIGRATION_MAP.md not found (MJCC-workspace) -- cannot audit coverage"
else
  missing=()
  for t in "${legacy_tables[@]}"; do
    if ! grep -q "$t" "$map"; then missing+=("$t"); fi
  done
  if [ ${#missing[@]} -gt 0 ]; then
    bad "rule 5: ${#missing[@]} legacy table(s) absent from MIGRATION_MAP.md: ${missing[*]}"
  else
    if awk '/\|/ {n++} END {exit !(n>0)}' "$map" && grep -qiE '\| *(migrated|transformed|archive-only|omitted)' "$map"; then
      ok "rule 5: every legacy table appears, with dispositions"
    else
      bad "rule 5: MIGRATION_MAP.md has no recognised disposition column (migrated|transformed|archive-only|omitted)"
    fi
  fi
fi

echo
if [ $fail -ne 0 ]; then
  echo "migration-static-check: FAIL"
  exit 1
fi
echo "migration-static-check: PASS"
