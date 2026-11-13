#!/usr/bin/env python3
"""Write a small set of fake Claude Code threads for the README demo.

Usage: python3 demo/make_demo.py <out-dir>

The threads go to <out-dir>/projects and their working folders to <out-dir>/work.
Point THREADS_ROOT at <out-dir>/projects and THREADS_INDEX at a scratch file.
"""

import json
import sys
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path

NOW = datetime.now(timezone.utc)

# (project, branch, hours ago, title, [(role, text), ...])
THREADS = [
    ("api-gateway", "feat/ABC-311-rate-limit", 2, "Add per-tenant rate limiting", [
        ("user", "Add per-tenant rate limiting to the gateway. Use a token bucket in Redis."),
        ("assistant", "I added a token bucket middleware. Each tenant gets 100 requests per second with a burst of 200."),
        ("user", "The limiter blocks health checks. Exclude /healthz from the bucket."),
    ]),
    ("api-gateway", "fix/ABC-298-timeouts", 30, "Upstream timeout retries", [
        ("user", "Requests to the billing upstream time out after 5 seconds. Add retries with backoff."),
        ("assistant", "Retries now use exponential backoff with jitter: 3 attempts, 200 ms base delay."),
    ]),
    ("web-app", "feat/ABC-305-dark-mode", 5, "Dark mode for the settings page", [
        ("user", "Add dark mode to the settings page. Follow the system preference."),
        ("assistant", "The settings page now reads prefers-color-scheme and stores an override in local storage."),
    ]),
    ("web-app", "fix/ABC-317-login-redirect", 50, "Login redirect loop", [
        ("user", "After login, Safari loops between /login and /dashboard."),
        ("assistant", "The session cookie had SameSite=Strict, so Safari dropped it on the redirect. It is Lax now."),
    ]),
    ("billing-service", "feat/ABC-322-invoice-pdf", 8, "Invoice PDF export", [
        ("user", "Export invoices as PDF with the company logo and line items."),
        ("assistant", "The export renders the invoice template to PDF. Totals round half to even, as the ledger does."),
    ]),
    ("billing-service", "fix/ABC-290-migration", 120, "Rollback for the currency migration", [
        ("user", "The currency column migration failed on staging. Write a rollback."),
        ("assistant", "The rollback restores the old column from the backup table, then drops the new one."),
        ("user", "Run the migration rollback on staging and check the row counts."),
    ]),
    ("infra", "main", 20, "Deploy pipeline for preview environments", [
        ("user", "Set up a deploy pipeline that creates a preview environment for every pull request."),
        ("assistant", "Each PR now gets a preview deployment. The pipeline deletes it when the PR closes."),
    ]),
    ("infra", "chore/ABC-301-terraform", 200, "Upgrade Terraform providers", [
        ("user", "Upgrade the AWS provider to the latest major version and fix the plan diffs."),
        ("assistant", "The plan is clean after the upgrade. Two resources moved to the new tagging block."),
    ]),
    ("mobile-app", "feat/ABC-330-push", 1, "Push notification opt-in", [
        ("user", "Ask for push notification permission after the first completed order, not on launch."),
        ("assistant", "The prompt now shows after the first order. Users who decline see it again after 30 days."),
    ]),
    ("mobile-app", "fix/ABC-327-crash", 70, "Crash on image upload", [
        ("user", "The app crashes when a user uploads a HEIC image on Android."),
        ("assistant", "The decoder did not handle HEIC. The upload converts it to JPEG first."),
    ]),
]


def slug(path: str) -> str:
    return "".join(c if c.isascii() and c.isalnum() else "-" for c in path)


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit("usage: make_demo.py <out-dir>")
    out = Path(sys.argv[1]).absolute()
    for project, branch, hours, title, messages in THREADS:
        cwd = out / "work" / project
        cwd.mkdir(parents=True, exist_ok=True)
        session = str(uuid.uuid4())
        folder = out / "projects" / slug(str(cwd))
        folder.mkdir(parents=True, exist_ok=True)
        start = NOW - timedelta(hours=hours)
        lines = []
        for i, (role, text) in enumerate(messages):
            content = text if role == "user" else [{"type": "text", "text": text}]
            lines.append({
                "type": role,
                "message": {"content": content},
                "timestamp": (start + timedelta(minutes=i)).isoformat().replace("+00:00", "Z"),
                "cwd": str(cwd),
                "gitBranch": branch,
                "entrypoint": "cli",
                "sessionId": session,
            })
        lines.append({"type": "ai-title", "aiTitle": title, "sessionId": session})
        with open(folder / f"{session}.jsonl", "w") as f:
            # Claude Code writes compact JSON, and the indexer's byte prefilter expects it.
            f.writelines(json.dumps(line, separators=(",", ":")) + "\n" for line in lines)


if __name__ == "__main__":
    main()
