#!/bin/bash
# Generate static CI status page at $PUBLIC_HTML/ci.html
#
# Called by the orchestrator after each build status change.

STATE_DIR="${STATE_DIR:-/home/aptbcachefsorg/package-ci}"
PUBLIC_HTML="${PUBLIC_HTML:-/home/aptbcachefsorg/public_html}"
GIT_REPO="${GIT_REPO:-/var/www/git/bcachefs-tools.git}"

DESIRED_FILE="$STATE_DIR/desired"
[ -f "$DESIRED_FILE" ] || exit 0
DESIRED="$(cat "$DESIRED_FILE")"

# A queued release. The page knew only about "desired", so a release that was
# never queued - or queued and not yet built - looked identical to nothing
# happening. That is how v1.39.0 sat misversioned in the release suite for a
# day with every job on this page green.
DESIRED_RELEASE="$(cat "$STATE_DIR/desired-release" 2>/dev/null)"

# sha -> tag, in one git call rather than a describe per commit: this runs on
# every status change and there are hundreds of build dirs. show-ref -d prints
# the annotated tag object and then the peeled commit as "<ref>^{}", so both
# the tag object and the commit resolve to the name.
declare -A TAGS
while read -r sha ref; do
    ref="${ref%^\{\}}"
    TAGS["$sha"]="${ref#refs/tags/}"
done < <(git --git-dir="$GIT_REPO" show-ref --tags -d 2>/dev/null)

OUTPUT="$PUBLIC_HTML/ci.html"
TMP="$OUTPUT.tmp$$"
# Don't strand the temp file when generate fails: there has been a
# ci.html.tmp662915 sitting in the web root since June.
trap 'rm -f "$TMP"' EXIT

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
  .release  { color: #6af; }
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
        [ -n "$DESIRED_RELEASE" ] && [ "$commit" = "$DESIRED_RELEASE" ] &&
            marker="$marker <span class='release'>&larr; release queued</span>"

        # Name the tag if there is one. Without this a release is
        # indistinguishable from any other commit on this page.
        tag="${TAGS[$commit]}"
        [ -n "$tag" ] && short="<span class='release'>$tag</span> $short"

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
