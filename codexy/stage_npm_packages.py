#!/usr/bin/env python3
"""Stage, pack and optionally publish the codexy npm packages.

codexy is published like @openai/codex: a small launcher package, plus one package per
platform holding the native Codex package directory. The platform packages are versions of
the same npm name (for example codexy@0.1.0-darwin-arm64) that the launcher installs as
optional-dependency aliases (codexy-darwin-arm64), so npm only downloads the one it needs.

Each --native argument is a canonical Codex package directory, or a .tar.gz of one, made by
scripts/build_codex_package.py.
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CODEX_CLI_ROOT = REPO_ROOT / "codex-cli"
PACKAGE_NAME = "codexy"
REPOSITORY = {
    "type": "git",
    "url": "git+https://github.com/huanyuqu/codex.git",
    "directory": "codex-cli",
}

# How bin/codex.js launches this package. The shared daemon is the official release, so
# codexy runs its own app-server. The -c overrides precede the user's, which win.
LAUNCH = {
    "embeddedServer": True,
    "config": [
        "features.context_graph=true",
        "features.context_graph_indexing=true",
        "suppress_unstable_features_warning=true",
    ],
}

# Rust target -> (npm tag, os, cpu). The tags match bin/codex.js.
PLATFORMS = {
    "aarch64-apple-darwin": ("darwin-arm64", "darwin", "arm64"),
    "x86_64-apple-darwin": ("darwin-x64", "darwin", "x64"),
    "aarch64-unknown-linux-musl": ("linux-arm64", "linux", "arm64"),
    "x86_64-unknown-linux-musl": ("linux-x64", "linux", "x64"),
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--version", required=True, help="Version of the codexy release.")
    parser.add_argument(
        "--native",
        action="append",
        default=[],
        metavar="TARGET=PATH",
        help="A Rust target and its Codex package directory or .tar.gz (repeatable).",
    )
    parser.add_argument(
        "--out-dir",
        type=Path,
        required=True,
        help="Directory for the packed .tgz files.",
    )
    parser.add_argument(
        "--publish",
        action="store_true",
        help="Publish the packed packages to npm, platform packages first.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="With --publish, pass --dry-run to npm publish.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    natives: dict[str, Path] = {}
    for spec in args.native:
        target, sep, path = spec.partition("=")
        if not sep or target not in PLATFORMS:
            raise SystemExit(
                f"--native takes TARGET=PATH with TARGET one of {', '.join(PLATFORMS)}: {spec}"
            )
        natives[target] = Path(path).resolve()
    if not natives:
        raise SystemExit("Provide at least one --native TARGET=PATH")

    out_dir = args.out_dir.resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    # Platform packages first: the launcher package depends on them.
    tarballs: list[tuple[Path, str | None]] = []
    with tempfile.TemporaryDirectory(prefix="codexy-npm-") as tmp:
        for target, source in natives.items():
            tag = PLATFORMS[target][0]
            staging = Path(tmp) / tag
            stage_platform_package(staging, args.version, target, source)
            tarballs.append((npm_pack(staging, out_dir), tag))
        staging = Path(tmp) / "launcher"
        stage_launcher_package(staging, args.version, sorted(natives))
        prerelease = "-" in args.version
        tarballs.append((npm_pack(staging, out_dir), "next" if prerelease else None))

    if args.publish:
        for tarball, tag in tarballs:
            command = ["npm", "publish", str(tarball), "--access", "public"]
            if tag:
                command += ["--tag", tag]
            if args.dry_run:
                command.append("--dry-run")
            run(command)
    return 0


def stage_platform_package(staging: Path, version: str, target: str, source: Path) -> None:
    tag, os_name, cpu = PLATFORMS[target]
    vendor = staging / "vendor" / target
    if source.is_dir():
        shutil.copytree(source, vendor, symlinks=True)
    else:
        vendor.mkdir(parents=True)
        with tarfile.open(source) as archive:
            if hasattr(tarfile, "data_filter"):
                archive.extractall(vendor, filter="data")
            else:
                archive.extractall(vendor)
    executable = vendor / "bin" / "codex"
    if not executable.is_file():
        raise SystemExit(f"{source} has no bin/codex; is it a Codex package for {target}?")
    write_json(
        staging / "package.json",
        {
            "name": PACKAGE_NAME,
            "version": f"{version}-{tag}",
            "description": f"The {tag} binaries of codexy.",
            "license": "Apache-2.0",
            "os": [os_name],
            "cpu": [cpu],
            "files": ["vendor", "NOTICE"],
            "repository": REPOSITORY,
        },
    )
    copy_legal_files(staging)


def stage_launcher_package(staging: Path, version: str, targets: list[str]) -> None:
    upstream = json.loads((CODEX_CLI_ROOT / "package.json").read_text(encoding="utf-8"))
    (staging / "bin").mkdir(parents=True)
    for name in upstream["files"]:
        shutil.copy2(CODEX_CLI_ROOT / name, staging / name)
    shutil.copy2(REPO_ROOT / "codexy" / "README.md", staging / "README.md")
    copy_legal_files(staging)
    write_json(
        staging / "package.json",
        {
            "name": PACKAGE_NAME,
            "version": version,
            "description": (
                "Codex CLI with a context graph, branches, merges and background commands "
                "(a fork of openai/codex)."
            ),
            "license": upstream.get("license", "Apache-2.0"),
            "bin": {PACKAGE_NAME: "bin/codex.js"},
            "type": upstream["type"],
            "engines": upstream["engines"],
            "os": sorted({PLATFORMS[target][1] for target in targets}),
            "cpu": sorted({PLATFORMS[target][2] for target in targets}),
            "files": [*upstream["files"], "NOTICE"],
            "repository": REPOSITORY,
            "optionalDependencies": {
                f"{PACKAGE_NAME}-{PLATFORMS[target][0]}": (
                    f"npm:{PACKAGE_NAME}@{version}-{PLATFORMS[target][0]}"
                )
                for target in targets
            },
            "codexLaunch": LAUNCH,
        },
    )


def copy_legal_files(staging: Path) -> None:
    for name in ("LICENSE", "NOTICE"):
        shutil.copy2(REPO_ROOT / name, staging / name)


def write_json(path: Path, value: dict) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def npm_pack(staging: Path, out_dir: Path) -> Path:
    output = subprocess.run(
        ["npm", "pack", "--json", "--pack-destination", str(out_dir)],
        cwd=staging,
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    ).stdout
    tarball = out_dir / json.loads(output)[0]["filename"]
    print(f"packed {tarball}", flush=True)
    return tarball


def run(command: list[str]) -> None:
    print("+", " ".join(command), flush=True)
    subprocess.run(command, check=True)


if __name__ == "__main__":
    sys.exit(main())
