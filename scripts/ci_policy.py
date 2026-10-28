"""Conservative CI routing and exact-source image publication checks."""
import json
import os
import subprocess
import sys


def docs_only(paths):
    return bool(paths) and all(
        path == "README.md"
        or (path.startswith("docs/") and path.endswith(".md"))
        for path in paths
    )


def trusted_run(run, sha):
    return (
        run["head_sha"] == sha
        and run["head_branch"] == "main"
        and run["event"] in ("push", "workflow_dispatch")
        and run["conclusion"] == "success"
    )


def full_ci_jobs(jobs):
    native = [job for job in jobs if job["name"].startswith("native (")]
    expected = {
        f"native ({os_name}, {rust})"
        for os_name in ("ubuntu-latest", "ubuntu-24.04-arm", "windows-latest", "macos-latest")
        for rust in ("1.89.0", "stable")
    }
    return (
        len(native) == 8
        and {job["name"] for job in native} == expected
        and all(job["conclusion"] == "success" for job in native)
        and all(
            any(job["name"] == name and job["conclusion"] == "success" for job in jobs)
            for name in ("lint", "docker")
        )
    )


def gh_pages(endpoint, field):
    pages = subprocess.check_output(
        ["gh", "api", "--paginate", "--slurp", endpoint], text=True
    )
    return [item for page in json.loads(pages) for item in page[field]]


def image_ci():
    if os.environ["GITHUB_REF"] != "refs/heads/main":
        raise RuntimeError("Deployment images must be dispatched on main.")
    repo, sha = os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_SHA"]
    runs = gh_pages(
        f"repos/{repo}/actions/workflows/ci.yml/runs"
        f"?head_sha={sha}&branch=main&status=success&per_page=100",
        "workflow_runs",
    )
    for run in runs:
        if trusted_run(run, sha):
            jobs = gh_pages(
                f"repos/{repo}/actions/runs/{run['id']}/attempts/{run['run_attempt']}"
                "/jobs?per_page=100", "jobs"
            )
            if full_ci_jobs(jobs):
                print(f"Verified full CI for {sha}: {run['html_url']}")
                with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as summary:
                    summary.write(f"Qualified CI: {run['html_url']}\n\n")
                return
    raise RuntimeError(
        "No successful full main CI run for this exact SHA. Wait for CI, or "
        "dispatch VLite CI on main (docs-only runs do not qualify), then retry."
    )


def route():
    with open(os.environ["GITHUB_EVENT_PATH"], encoding="utf-8") as source:
        event = json.load(source)
    kind = os.environ["GITHUB_EVENT_NAME"]
    full = True
    if kind in ("push", "pull_request"):
        base = event["before"] if kind == "push" else event["pull_request"]["base"]["sha"]
        if base and set(base) != {"0"}:
            paths = subprocess.check_output(
                ["git", "diff", "--name-only", "-z", base, os.environ["GITHUB_SHA"]]
            ).decode("utf-8").rstrip("\0").split("\0")
            full = not docs_only(paths)
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        output.write(f"full={str(full).lower()}\n")
    print("Full native/container matrix" if full else "Markdown-only documentation checks")


if __name__ == "__main__":
    {"route": route, "image-ci": image_ci}[sys.argv[1]]()
