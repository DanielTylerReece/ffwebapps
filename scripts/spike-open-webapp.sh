#!/usr/bin/env bash
#
# Phase 0 spike: prove that Firefox's first-party "Web Apps" (Taskbar Tabs)
# infrastructure produces a self-contained app window on Linux, driven purely
# from the command line + a pref — no chrome monkeypatching, no source patch.
#
# It launches a *throwaway* profile with `--no-remote` so it cannot disturb a
# running Firefox/Nightly session or any existing PWA profile.
#
# Usage:  scripts/spike-open-webapp.sh [URL]
#         FF=firefox scripts/spike-open-webapp.sh https://teams.microsoft.com/
#
set -euo pipefail

FF="${FF:-firefox-nightly}"
URL="${1:-https://example.com/}"
PROFILE="$(mktemp -d /tmp/ffwebapps-spike.XXXXXX)"
LOG="$PROFILE/spike.log"
ID="$(cat /proc/sys/kernel/random/uuid)"

# Minimal prefs: enable the feature + silence first-run/telemetry noise.
cat > "$PROFILE/user.js" <<'EOF'
user_pref("browser.taskbarTabs.enabled", true);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
EOF

echo "== ffwebapps Phase 0 spike =="
echo "FF      : $FF  ($($FF --version 2>/dev/null || echo '??'))"
echo "Profile : $PROFILE"
echo "App id  : $ID"
echo "URL     : $URL"
echo

# The exact argument shape Firefox's own shortcut uses (see TaskbarTabsCmd.sys.mjs):
#   -taskbar-tab <id> -new-window <url> -container <userContextId>
nohup "$FF" --no-remote -profile "$PROFILE" \
  -taskbar-tab "$ID" -new-window "$URL" -container 0 \
  > "$LOG" 2>&1 &
PID=$!
disown "$PID" 2>/dev/null || true
echo "Launched detached PID $PID"

# Give it time to start (this script is intended to be run in the background).
sleep 6

echo
echo "== process =="
if kill -0 "$PID" 2>/dev/null; then
  echo "PID $PID is alive"
  pgrep -af "taskbar-tab $ID" || true
else
  echo "PID $PID exited early — see log below"
fi

echo
echo "== window identity (best-effort across compositors) =="
{
  command -v wmctrl   >/dev/null && wmctrl -lx 2>/dev/null | grep -i "webapp\|taskbartab\|$ID"
  command -v hyprctl  >/dev/null && hyprctl clients -j 2>/dev/null | grep -i "webapp\|class"
  command -v swaymsg  >/dev/null && swaymsg -t get_tree 2>/dev/null | grep -i "app_id.*webapp"
  command -v xprop    >/dev/null && command -v xdotool >/dev/null && \
    for w in $(xdotool search --name "" 2>/dev/null); do xprop -id "$w" WM_CLASS 2>/dev/null | grep -i webapp; done
} 2>/dev/null | sort -u || echo "(no window-introspection tool matched; check visually)"

echo
echo "== relevant log lines =="
grep -iE "taskbartab|web app|webapp|displayMode|error|warn" "$LOG" 2>/dev/null | tail -20 || true

echo
echo "Profile + log kept at: $PROFILE"
echo "Clean up with:  rm -rf '$PROFILE'  (and close the app window)"
