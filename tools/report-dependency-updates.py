#!/usr/bin/env python3
"""Report registry dependency requirements and latest stable releases.

This is informational, like cargo-outdated's default exit behavior. Fetch each
crate's metadata independently: newest pyo3, numpy and pyo3-stub-gen releases need
not support a simultaneous upgrade. Metadata/registry failures still fail the job.
"""
import json
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request

CRATES_IO = {"registry+https://github.com/rust-lang/crates.io-index",
             "sparse+https://index.crates.io/"}


def latest_version(name):
    request = urllib.request.Request(
        "https://crates.io/api/v1/crates/" + urllib.parse.quote(name, safe=""),
        headers={"User-Agent": "adb-client-dependency-report/1.0 (https://github.com/pawelchcki/adb_client)"},
    )
    for attempt in range(3):
        try:
            with urllib.request.urlopen(request, timeout=20) as response:
                crate = json.load(response)["crate"]
            version = crate.get("max_stable_version") or crate["max_version"]
            if not version:
                raise ValueError(f"Registry returned no release for {name}")
            return version
        except urllib.error.HTTPError as error:
            if error.code not in {429, 500, 502, 503, 504} or attempt == 2:
                raise
            time.sleep(2 ** attempt)
    raise RuntimeError(f"Could not query {name}")


def main():
    result = subprocess.run(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version=1"],
        text=True, stdout=subprocess.PIPE, check=True,
    )
    metadata = json.loads(result.stdout)
    members = set(metadata["workspace_members"])
    rows = sorted({
        (package["name"], dependency["name"], dependency["req"],
         dependency.get("kind") or "normal", dependency.get("target") or "all")
        for package in metadata["packages"] if package["id"] in members
        for dependency in package["dependencies"] if dependency.get("source") in CRATES_IO
    })
    versions = {}
    for name in sorted({row[1] for row in rows}):
        versions[name] = latest_version(name)
        time.sleep(0.1)
    print("Package\tDependency\tDeclared requirement\tLatest stable\tKind\tTarget")
    for package, dependency, requirement, kind, target in rows:
        print(f"{package}\t{dependency}\t{requirement}\t{versions[dependency]}\t{kind}\t{target}")


if __name__ == "__main__":
    main()
