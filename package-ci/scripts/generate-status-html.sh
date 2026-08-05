#!/bin/bash
# Generate static CI status page at $PUBLIC_HTML/ci.html
#
# Called by the orchestrator after each build status change.

STATE_DIR="${STATE_DIR:-/home/aptbcachefsorg/package-ci}"
PUBLIC_HTML="${PUBLIC_HTML:-/home/aptbcachefsorg/public_html}"

DESIRED_FILE="$STATE_DIR/desired"
[ -f "$DESIRED_FILE" ] || exit 0
DESIRED="$(cat "$DESIRED_FILE")"

OUTPUT="$PUBLIC_HTML/ci.html"
TMP="$OUTPUT.tmp$$"

generate() {
    cat << 'EOF'
<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta http-equiv="refresh" content="30">
<title>bcachefs-tools CI</title>
<style>
  body { font-family: monospace; background: #1a1a1a; color: #ccc; padding: 2em; }
  h1 { color: #fff; font-size: 1.2em; }
  table { border-collapse: collapse; margin-top: 1em; }
  td, th { padding: 0.3em 1.2em 0.3em 0; text-align: left; }
  th { color: #888; font-weight: normal; border-bottom: 1px solid #333; }
  .done     { color: #5f5; }
  .failed   { color: #f55; }
  .building { color: #fa0; }
  .pending  { color: #888; }
  .summary  { margin-top: 1em; color: #888; }
</style>
</head>
<body>
<h1>bcachefs-tools CI</h1>
EOF

    for commit_dir in $(ls -dt "$STATE_DIR/builds"/*/); do
        commit="$(basename "$commit_dir")"
        short="${commit:0:12}"
        marker=""
        [ "$commit" = "$DESIRED" ] && marker=" &larr; desired"

        echo "<p style='color:#aaa;font-size:0.9em'>commit $short$marker</p>"
        echo "<table>"
        echo "<tr><th>job</th><th>status</th><th></th></tr>"

        total=0; ndone=0; nfailed=0; nbuilding=0
        for job_dir in $(ls -d "$commit_dir"*/); do
            job="$(basename "$job_dir")"
            [ "$job" = "source" ] && continue
            status="$(cat "$job_dir/status" 2>/dev/null || echo pending)"
            ((total++))
            case "$status" in
                done)     ((ndone++));     sym="&#10003;" ;;
                failed)   ((nfailed++));   sym="&#10007;" ;;
                building) ((nbuilding++)); sym="&#8230;" ;;
                *)                         sym="&middot;" ;;
            esac
            echo "<tr><td>$job</td><td class='$status'>$sym $status</td><td><a href='/ci-builds/$commit/$job/log' style='color:#666'>log</a></td></tr>"
        done

        echo "</table>"
        echo -n "<p class='summary'>$ndone/$total done"
        [ "$nfailed"   -gt 0 ] && echo -n ", <span class='failed'>$nfailed failed</span>"
        [ "$nbuilding" -gt 0 ] && echo -n ", <span class='building'>$nbuilding building</span>"
        echo "</p>"
    done

    UPDATED="$(date -u '+%Y-%m-%d %H:%M UTC')"
    echo "<p style='margin-top:2em;color:#555;font-size:0.8em'>updated $UPDATED &middot; refreshes every 30s</p>"
    echo "</body></html>"
}

generate > "$TMP" && mv "$TMP" "$OUTPUT"
