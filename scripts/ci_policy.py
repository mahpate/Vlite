"""Conservative CI routing and exact-source image publication checks."""
import json
import os
import re
import subprocess
import sys


def docs_only(paths):
    return bool(paths) and all(
        path == "README.md"
        or (path.startswith("docs/") and path.endswith(".md"))
        for path in paths
    )


# The Docker job rebuilds the image from source and runs the whole suite inside it, which is
# the longest job of a run. It follows the files that can change its outcome: the image and
# compose definitions, the deployment assets, the container helper scripts, and the end-to-end
# scripts that only this job runs. Source changes are covered by the native matrix, and every
# release still runs the container suite (release.yml, image-test).
DOCKER_FILES = frozenset({
    "Dockerfile", ".dockerignore", "tests/docker_setup.py", ".github/workflows/ci.yml",
    "tests/rest_e2e.py", "tests/tls_e2e.py", "tests/dashboard_e2e.py", "tests/requirements-dashboard.txt",
})
DOCKER_PREFIXES = ("deploy/", "scripts/docker")
COMPOSE_FILE = re.compile(r"(docker-)?compose(\.[\w-]+)?\.ya?ml")


def touches_docker(paths):
    return any(
        path in DOCKER_FILES or path.startswith(DOCKER_PREFIXES) or COMPOSE_FILE.fullmatch(path)
        for path in paths
    )


def trusted_run(run, sha):
    return (
        run["head_sha"] == sha
        and run["head_branch"] == "main"
        and run["event"] in ("push", "workflow_dispatch")
        and run["conclusion"] == "success"
    )


NATIVE_OSES = ("ubuntu-latest", "ubuntu-24.04-arm", "windows-latest", "macos-latest")
TOOLCHAINS = ("1.89.0", "stable")
# The MSRV runs the Windows suite nowhere: native (windows-latest, 1.89.0) only checks it.
WINDOWS_TOOLCHAINS = ("stable",)
WINDOWS_SHARDS = 3


def all_succeeded(jobs, prefix, expected):
    """Exactly the expected job names carry the prefix, and every one of them succeeded."""
    found = [job for job in jobs if job["name"].startswith(prefix)]
    return (
        len(found) == len(expected)
        and {job["name"] for job in found} == expected
        and all(job["conclusion"] == "success" for job in found)
    )


def full_ci_jobs(jobs):
    native = {f"native ({os_name}, {rust})" for os_name in NATIVE_OSES for rust in TOOLCHAINS}
    windows = {
        f"windows-tests ({rust}, shard {shard}/{WINDOWS_SHARDS})"
        for rust in WINDOWS_TOOLCHAINS
        for shard in range(1, WINDOWS_SHARDS + 1)
    }
    return (
        all_succeeded(jobs, "native (", native)
        and all_succeeded(jobs, "windows-tests (", windows)
        and all(
            any(job["name"] == name and job["conclusion"] == "success" for job in jobs)
            for name in ("lint", "docker", "default-features")
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
        "dispatch VLite CI on main (docs-only runs do not qualify), then retry. "
        "A run qualifies only if its Docker job succeeded: a push that changed no container "
        "file skips that job, so dispatch VLite CI on main with the Docker job enabled."
    )


def release_run(run, sha):
    # A pull request run tests the merge of its head, never the head itself, so it cannot vouch for it.
    return (
        run["head_sha"] == sha
        and run["event"] in ("push", "workflow_dispatch")
        and run["conclusion"] == "success"
    )


# A release is built with stable. When ci.yml already ran the stable suites on this exact commit,
# the release skips its own native test runs (release.yml, ci-green). Anything short of a green
# run of every one of these jobs means the release runs the tests itself, as it always did.
RELEASE_TEST_JOBS = frozenset(
    {f"native ({os_name}, stable)" for os_name in NATIVE_OSES}
    | {f"windows-tests (stable, shard {shard}/{WINDOWS_SHARDS})" for shard in range(1, WINDOWS_SHARDS + 1)}
)


def release_tests_ran(jobs):
    for name in RELEASE_TEST_JOBS:
        found = [job for job in jobs if job["name"] == name]
        if not found or any(job["conclusion"] != "success" for job in found):
            return False
    return True


def release_ci():
    """Write green=true when a successful full CI run of this exact commit can stand in for the tests."""
    repo, sha = os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_SHA"]
    green, detail = False, "no successful CI run of this commit was found"
    try:
        runs = gh_pages(
            f"repos/{repo}/actions/workflows/ci.yml/runs"
            f"?head_sha={sha}&status=success&per_page=100",
            "workflow_runs",
        )
        for run in runs:
            if not release_run(run, sha):
                continue
            jobs = gh_pages(
                f"repos/{repo}/actions/runs/{run['id']}/attempts/{run['run_attempt']}"
                "/jobs?per_page=100", "jobs"
            )
            if release_tests_ran(jobs):
                green, detail = True, run["html_url"]
                break
            detail = "the successful CI runs of this commit skipped some stable test jobs"
    except Exception as error:  # Whatever goes wrong, the safe answer is to run the tests.
        detail = f"the Actions API could not be read ({type(error).__name__})"
    message = (
        f"CI is green for {sha} ({detail}); the native test suites are not repeated."
        if green
        else f"No qualifying CI run for {sha} ({detail}); the native test suites run in this release."
    )
    print(message)
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        output.write(f"green={str(green).lower()}\n")
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as summary:
        summary.write(message + "\n\n")


def route():
    with open(os.environ["GITHUB_EVENT_PATH"], encoding="utf-8") as source:
        event = json.load(source)
    kind = os.environ["GITHUB_EVENT_NAME"]
    # Unknown changes (a new branch, a manual run) take the whole matrix, Docker job included.
    full = docker = True
    if kind in ("push", "pull_request"):
        base = event["before"] if kind == "push" else event["pull_request"]["base"]["sha"]
        if base and set(base) != {"0"}:
            paths = subprocess.check_output(
                ["git", "diff", "--name-only", "-z", base, os.environ["GITHUB_SHA"]]
            ).decode("utf-8").rstrip("\0").split("\0")
            full = not docs_only(paths)
            docker = full and touches_docker(paths)
    elif os.environ.get("CI_DOCKER_INPUT", "").lower() == "false":
        # Only a manual run can leave the container suite out, for a quicker look at the rest.
        docker = False
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        output.write(f"full={str(full).lower()}\ndocker={str(docker).lower()}\n")
    if not full:
        print("Markdown-only documentation checks")
    elif docker:
        print("Full native/container matrix")
    else:
        print("Full native matrix; the Docker job is skipped (no container file changed)")


if __name__ == "__main__":
    {"route": route, "image-ci": image_ci, "release-ci": release_ci}[sys.argv[1]]()
