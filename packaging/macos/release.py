#!/usr/bin/env python3
"""Apple Silicon macOS release builder for Traverse. Python 3.9+, standard library only.

  doctor    check the toolchain
  build     compile a clean source snapshot, stage it, ad-hoc sign and smoke-test it;
            needs no signing identity or credentials
  finalize  Developer ID-sign a build, notarize it, then write the tarballs and checksums
  verify    re-check a finalized release: signatures, notarization tickets, checksums

The signing identity and notary profile are arguments, never stored in this file.
Nothing is installed or published; finalize sends the binaries to Apple's notary
service and nowhere else.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time

# -- Project ---------------------------------------------------------------
PROJECT = "traverse"
ORIGINS = tuple(form.format("truespar/traverse") for form in (
    "git@github.com:{}.git", "https://github.com/{}.git", "https://github.com/{}",
    "ssh://git@github.com/{}.git"))
NEEDS_PROTOC = True  # traverse-server's grpc feature runs tonic-prost-build
CARGO_BUILDS = (
    ("-p", "traverse-server",
     "--features", ",".join("traverse-server/" + f for f in ("grpc", "tls", "mcp", "ml", "gql")),
     "-p", "traverse-cli", "-p", "traverse-admin", "-p", "traverse-bench"),
    ("-p", "traverse-ffi", "-p", "traverse-java"),
)
LICENSES = ("LICENSE", "LICENSE-APACHE", "LICENSE-MIT", "THIRD-PARTY-NOTICES")
# (archive name, built files, repository files) - the same assets the Linux and
# Windows releases carry, under the macOS name.
ARCHIVES = (
    ("traverse-{version}-aarch64-macos",
     ("traverse-server", "traverse-cli", "traverse-admin", "traverse-bench"),
     LICENSES + ("README.md", "RELEASE_NOTES.md")),
    ("traverse-ffi-{version}-aarch64-macos", ("libtraverse_ffi.dylib",),
     LICENSES + ("crates/traverse-ffi/traverse.h",)),
)
# Signed and notarized libraries for the language packages (.NET, Go, Java).
# Not a release asset; they go into those packages' osx-arm64 / macos slots.
NATIVE = ("{project}-native-{version}-aarch64-macos",
          ("libtraverse_ffi.dylib", "libtraverse_java.dylib"), LICENSES)
SMOKE_ARGS = ("--version",)
# --------------------------------------------------------------------------

ROOT = Path(__file__).resolve().parents[2]
TARGET = "aarch64-apple-darwin"
MIN_OS = "13.0"
MACHO_MAGIC = (b"\xcf\xfa\xed\xfe", b"\xca\xfe\xba\xbe")  # arm64/x86_64 thin, universal
SYSTEM_LIBS = ("/usr/lib/", "/System/Library/")


class ReleaseError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise ReleaseError(message)


def run(*args, cwd=None, env=None, capture=False, timeout=None):
    args = [str(arg) for arg in args]
    print("+ " + " ".join(args), flush=True)
    result = subprocess.run(args, cwd=cwd, env=env, check=True, text=True, timeout=timeout,
                            stdout=subprocess.PIPE if capture else None)
    return result.stdout.strip() if capture else None


def quiet(*args):
    """Run without echoing; returns (exit code, stdout + stderr)."""
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True)
    return result.returncode, result.stdout + result.stderr


def git(root, *args):
    return run("git", "-C", root, *args, capture=True)


def digest(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            value.update(block)
    return value.hexdigest()


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def cargo_field(root, field):
    # [workspace.package] first, then [package]; no TOML parser on a stock Mac.
    cargo = (root / "Cargo.toml").read_text()
    for section in ("[workspace.package]", "[package]"):
        if section in cargo:
            body = cargo.split(section, 1)[1].split("\n[", 1)[0]
            match = re.search(r'^' + field + r'\s*=\s*"([^"]+)"\s*$', body, re.M)
            if match:
                return match.group(1)
    return None


def numbers(text):
    return tuple(int(part) for part in re.findall(r"\d+", text)[:3])


def machos(root):
    found = []
    for path in sorted(Path(root).rglob("*")):
        if path.is_file() and not path.is_symlink():
            with path.open("rb") as stream:
                if stream.read(4) in MACHO_MAGIC:
                    found.append(path)
    return found


def inventory(root):
    result = {}
    for path in sorted(Path(root).rglob("*")):
        require(not path.is_symlink(), "Unexpected symlink in package: " + str(path))
        if path.is_file():
            result[path.relative_to(root).as_posix()] = {
                "sha256": digest(path), "size": path.stat().st_size,
                "mode": stat.S_IMODE(path.stat().st_mode)}
    return result


def protoc():
    return os.environ.get("PROTOC") or shutil.which("protoc")


def doctor(root):
    require(sys.platform == "darwin" and os.uname().machine == "arm64",
            "This release target requires an Apple Silicon Mac.")
    for program in ("git", "cargo", "rustc", "codesign", "security", "ditto", "lipo", "otool",
                    "install_name_tool", "tar", "xcrun"):
        require(shutil.which(program), "Missing tool: " + program)
    tools = {"macOS": run("sw_vers", "-productVersion", capture=True),
             "Xcode": run("xcodebuild", "-version", capture=True).splitlines()[0],
             "SDK": run("xcrun", "--show-sdk-version", capture=True),
             "notarytool": run("xcrun", "notarytool", "--version", capture=True),
             "rustc": run("rustc", "-V", cwd=root, capture=True),
             "cargo": run("cargo", "-V", cwd=root, capture=True)}
    floor = cargo_field(root, "rust-version")
    if floor:
        require(numbers(tools["rustc"]) >= numbers(floor),
                "rustc is older than the rust-version " + floor + " in Cargo.toml. "
                "Update the toolchain or point RUSTUP_TOOLCHAIN at a newer one.")
    if NEEDS_PROTOC:
        require(protoc(), "protoc is required: put it on PATH or set PROTOC.")
        tools["protoc"] = run(protoc(), "--version", capture=True)
    print(json.dumps(tools, indent=2))
    return tools


def public_source(root):
    origin = git(root, "remote", "get-url", "origin")
    require(origin in ORIGINS, "Release source must be the public truespar/" + PROJECT + " repository.")
    return origin


def snapshot(root, source, allow_dirty):
    # Build from a fresh clone of the commit, never from the working tree, so
    # nothing generated, ignored or local can end up in a release.
    origin = public_source(root)
    dirty = bool(git(root, "status", "--porcelain", "--untracked-files=all"))
    require(not dirty or allow_dirty, "Working tree is dirty. --allow-dirty is for unsigned candidates only.")
    commit = git(root, "rev-parse", "HEAD")
    public_commit = bool(git(root, "for-each-ref", "--format=%(refname)", "--contains", commit,
                             "refs/remotes/origin/"))
    run("git", "clone", "--quiet", "--no-local", "--no-checkout", root, source)
    run("git", "-C", source, "checkout", "--quiet", "--detach", commit)
    run("git", "-C", source, "remote", "set-url", "origin", origin)
    if dirty:
        listed = subprocess.check_output(["git", "-C", str(root), "ls-files", "-z", "--cached",
                                          "--others", "--exclude-standard"]).decode()
        for name in sorted(set(filter(None, listed.split("\0")))):
            original, copied = root / name, source / name
            require(not original.is_symlink(), "Source symlinks need a release review: " + name)
            if original.is_file():
                copied.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(original, copied)
            elif copied.is_file():
                copied.unlink()  # a tracked deletion, inside the snapshot only
    return {"origin": origin, "commit": commit, "dirty": dirty, "public_commit": public_commit}


def build_environment(work):
    # Only what the toolchain needs; no inherited RUSTFLAGS, compiler flags or
    # loader injection from the calling shell.
    allowed = ("PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "DEVELOPER_DIR", "SDKROOT",
               "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "CARGO_HOME")
    env = {key: os.environ[key] for key in allowed if key in os.environ}
    if NEEDS_PROTOC:
        env["PROTOC"] = protoc()
    home = Path.home()
    cargo_home = Path(env.get("CARGO_HOME", home / ".cargo"))
    rustup_home = Path(env.get("RUSTUP_HOME", home / ".rustup"))
    env.update(MACOSX_DEPLOYMENT_TARGET=MIN_OS, CARGO_TARGET_DIR=str(work / "cargo"),
               CI="1", GIT_TERMINAL_PROMPT="0", COPYFILE_DISABLE="1")
    # rustc applies the last matching prefix, so the broadest goes first. The
    # work dir holds both the source snapshot and the target dir (generated
    # code such as the gRPC stubs is include!d from there). This variable also
    # replaces any build.rustflags from .cargo/config.toml - deliberately: a
    # shipped binary gets the apple-m1 baseline, never target-cpu=native.
    remaps = ((home, "/home"), (cargo_home, "/cargo"), (rustup_home, "/rustup"), (work, "/build"))
    env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(
        "--remap-path-prefix=%s=%s" % (path, name) for path, name in remaps)
    return env


def stage_files(source, rust, stage, version):
    archives = []
    for name, built, files in ARCHIVES + (NATIVE,):
        folder = stage / name.format(project=PROJECT, version=version)
        folder.mkdir(parents=True)
        for item in built:
            shutil.copy2(rust / item, folder / item)
        for item in files:
            shutil.copy2(source / item, folder / Path(item).name)
        archives.append(folder.name)
    return archives[:-1], archives[-1]


def fix_install_names(stage):
    # cargo leaves the build directory in a dylib's install name; a library
    # that ships must not name the machine that built it.
    for path in machos(stage):
        if path.suffix == ".dylib":
            run("install_name_tool", "-id", "@rpath/" + path.name, path)


def sign(stage, identity):
    # Hardened Runtime with no entitlements: nothing here JITs or needs a
    # protected resource. Ad-hoc ("-") only for the unsigned candidate.
    options = ["--force", "--sign", identity, "--options", "runtime"]
    options.append("--timestamp" if identity != "-" else "--timestamp=none")
    for path in machos(stage):
        run("codesign", *options, path)
        run("codesign", "--verify", "--strict", path)


def check_portability(stage, work):
    leaks = sorted({str(Path.home()), str(work)})
    for path in machos(stage):
        require(run("lipo", "-archs", path, capture=True) == "arm64", "Not arm64-only: " + str(path))
        for line in run("otool", "-L", path, capture=True).splitlines()[1:]:
            dependency = line.strip().split(" (", 1)[0]
            if dependency == "@rpath/" + path.name:
                continue  # a dylib's own install name; nothing else may need a bundled library
            require(dependency.startswith(SYSTEM_LIBS), "Nonportable dependency in %s: %s" % (path.name, dependency))
        commands = run("otool", "-l", path, capture=True)
        minimums = re.findall(r"^\s*minos\s+(\S+)\s*$", commands, re.M)
        require(len(minimums) == 1 and numbers(minimums[0]) <= numbers(MIN_OS),
                "Unexpected deployment target in %s: %s" % (path.name, minimums))
        for rpath in re.findall(r"cmd LC_RPATH\n.*?\n\s*path (.+) \(offset", commands, re.S):
            require(rpath.startswith(("@", "/usr/lib/", "/System/")), "Build-machine rpath: " + rpath)
        data = path.read_bytes()
        for leak in leaks:
            require(leak.encode() not in data, "Build-machine path %s inside %s" % (leak, path.name))


def smoke(stage, version):
    with tempfile.TemporaryDirectory(prefix=PROJECT + "-release-smoke-") as temp:
        for path in machos(stage):
            if path.suffix == ".dylib":
                run(sys.executable, "-I", "-c", "import ctypes, sys; ctypes.CDLL(sys.argv[1])", path,
                    cwd=temp, timeout=60)
            else:
                output = run(path, *SMOKE_ARGS, cwd=temp, capture=True, timeout=60)
                require("--version" not in SMOKE_ARGS or version in output,
                        "%s does not report version %s" % (path.name, version))


def build(args):
    root = args.repo.resolve()
    tools = doctor(root)
    output = args.output.expanduser().resolve()
    require(not output.is_relative_to(root), "Choose an output directory outside the checkout.")
    require(not output.exists(), "Choose a new output directory; builds are never overwritten.")
    output.mkdir(parents=True)
    work = output / "work"
    source = work / "source"
    work.mkdir()
    provenance = snapshot(root, source, args.allow_dirty)
    version = cargo_field(source, "version")
    require(version and re.fullmatch(r"\d+\.\d+\.\d+", version), "A numeric x.y.z version is required.")
    env = build_environment(work)
    lock = digest(source / "Cargo.lock")
    for packages in CARGO_BUILDS:
        run("cargo", "build", "--locked", "--release", "--target", TARGET, *packages, cwd=source, env=env)
    require(digest(source / "Cargo.lock") == lock, "Cargo.lock changed during the build.")
    stage = output / "stage"
    archives, native = stage_files(source, work / "cargo" / TARGET / "release", stage, version)
    fix_install_names(stage)
    sign(stage, "-")
    check_portability(stage, work)
    smoke(stage, version)
    write_json(output / "manifest.json", {
        "schema": 1, "project": PROJECT, "status": "unsigned-candidate", "version": version,
        "target": TARGET, "minimum_macos": MIN_OS, "source": provenance, "tools": tools,
        "cargo_lock": lock, "archives": archives, "native": native, "stage": inventory(stage)})
    print("Unsigned candidate: %s\nNot for distribution. Nothing installed or published." % output)


def identity_team(identity):
    match = re.fullmatch(r"Developer ID Application: .+ \(([A-Z0-9]{10})\)", identity)
    require(match is not None, "Expected the full 'Developer ID Application: NAME (TEAMID)' identity.")
    return match.group(1)


def signature_info(path):
    code, info = quiet("codesign", "-dvvv", path)
    require(code == 0, "Unreadable signature: " + str(path))
    return info


def cdhash(path):
    match = re.search(r"^CDHash=([0-9a-f]+)$", signature_info(path), re.M)
    require(match is not None, "No CDHash for " + str(path))
    return match.group(1)


def check_signatures(stage, team):
    for path in machos(stage):
        info = signature_info(path)
        flags = re.search(r"flags=0x[0-9a-f]+\(([^)]*)\)", info)
        require("Authority=Developer ID Application:" in info and "TeamIdentifier=" + team in info,
                "Not signed by team %s: %s" % (team, path))
        require(flags is not None and "runtime" in flags.group(1), "No Hardened Runtime: " + str(path))
        require(re.search(r"^Timestamp=", info, re.M), "No secure timestamp: " + str(path))


def submit(archive, profile, logs):
    result = json.loads(run("xcrun", "notarytool", "submit", archive, "--keychain-profile", profile,
                            "--output-format", "json", capture=True))
    # Persist the submission before waiting, so an interruption cannot lose its ID.
    write_json(logs / (archive.stem + ".submission.json"), result)
    require(result.get("id"), "Notarization returned no submission ID.")
    return result["id"]


def await_notarization(submission, folder, profile, logs):
    result = json.loads(run("xcrun", "notarytool", "wait", submission, "--keychain-profile", profile,
                            "--timeout", "30m", "--output-format", "json", capture=True))
    write_json(logs / (folder.name + ".result.json"), result)
    log = logs / (folder.name + ".notary-log.json")
    run("xcrun", "notarytool", "log", submission, "--keychain-profile", profile, log)
    require(result.get("status") == "Accepted", "Notarization was not accepted; see " + str(log))
    tickets = [entry["cdhash"] for entry in json.loads(log.read_text()).get("ticketContents") or []]
    for path in machos(folder):
        local = cdhash(path)
        require(any(ticket.startswith(local) for ticket in tickets),
                "The notarization ticket does not cover " + str(path))


def await_tickets(stage, minutes):
    # Apple answers "Accepted" before Gatekeeper's online lookup can see the
    # ticket (not yet after 1.5 minutes, there within 9). A bare Mach-O cannot be stapled, so a
    # downloaded copy depends on that lookup - publishing earlier would have
    # first users blocked as "Unnotarized Developer ID".
    pending = machos(stage)
    deadline = time.time() + minutes * 60
    while True:
        pending = [path for path in pending
                   if quiet("codesign", "--verify", "--check-notarization", "-R=notarized", path)[0] != 0]
        print("Gatekeeper tickets visible: %d of %d" % (len(machos(stage)) - len(pending), len(machos(stage))),
              flush=True)
        if not pending:
            return
        if time.time() > deadline:
            raise ReleaseError("Apple accepted the release but Gatekeeper cannot see %d ticket(s) yet. "
                               "Run `verify` later; do not publish until it passes." % len(pending))
        time.sleep(30)


def package(stage, destination, archives):
    destination.mkdir()
    artifacts = []
    for name in archives:
        archive = destination / (name + ".tar.gz")
        # Owner root:root like the Linux release; no xattrs or AppleDouble files.
        run("tar", "--no-mac-metadata", "--no-xattrs", "--uid", "0", "--gid", "0", "--uname", "root",
            "--gname", "root", "-czf", archive, "-C", stage, name,
            env=dict(os.environ, COPYFILE_DISABLE="1"))
        (destination / (archive.name + ".sha256")).write_text(digest(archive) + "  " + archive.name + "\n")
        artifacts.append(archive)
    (destination / "SHA256SUMS").write_text("".join(digest(p) + "  " + p.name + "\n" for p in artifacts))
    return {path.name: digest(path) for path in artifacts}


def validate_candidate(candidate):
    manifest = json.loads((candidate / "manifest.json").read_text())
    require(manifest.get("schema") == 1 and manifest.get("project") == PROJECT
            and manifest.get("status") == "unsigned-candidate", "Not an unsigned %s candidate." % PROJECT)
    source = manifest["source"]
    require(source["origin"] in ORIGINS and not source["dirty"] and source["public_commit"],
            "Signing requires a clean build of a commit that is on the public remote.")
    require(manifest["stage"] == inventory(candidate / "stage"), "Candidate files changed after the build.")
    return manifest


def finalize(args):
    require(sys.platform == "darwin", "Signing requires macOS.")
    team = identity_team(args.identity)
    identities = run("security", "find-identity", "-v", "-p", "codesigning", capture=True)
    require('"' + args.identity + '"' in identities, "No valid identity with a private key: " + args.identity)
    candidate = args.input.expanduser().resolve()
    manifest = validate_candidate(candidate)
    release = candidate / "release"
    require(not release.exists(), "Release output already exists; build a fresh candidate to retry.")
    release.mkdir()
    stage = release / "stage"
    shutil.copytree(candidate / "stage", stage)
    sign(stage, args.identity)
    check_signatures(stage, team)
    logs = release / "notarization"
    logs.mkdir()
    submissions = {}
    for name in manifest["archives"] + [manifest["native"]]:
        upload = logs / (name + ".zip")
        run("ditto", "-c", "-k", "--keepParent", stage / name, upload)
        submissions[name] = submit(upload, args.notary_profile, logs)
    for name, submission in submissions.items():
        await_notarization(submission, stage / name, args.notary_profile, logs)
    artifacts = package(stage, release / "artifacts", manifest["archives"])
    shutil.copytree(stage / manifest["native"], release / "native")
    manifest.update(status="notarized-tickets-pending", team=team, submissions=submissions,
                    artifacts=artifacts, stage=inventory(stage))
    write_json(release / "manifest.json", manifest)
    await_tickets(stage, args.ticket_timeout)
    manifest["status"] = "notarized"
    write_json(release / "manifest.json", manifest)
    print("Notarized: %s\nNative libraries: %s\nNothing published."
          % (release / "artifacts", release / "native"))


def verify(args):
    release = args.input.expanduser().resolve() / "release"
    manifest = json.loads((release / "manifest.json").read_text())
    require(manifest.get("project") == PROJECT and manifest.get("status", "").startswith("notarized"),
            "Not a finalized %s release." % PROJECT)
    stage = release / "stage"
    require(manifest["stage"] == inventory(stage), "Release files changed after signing.")
    check_signatures(stage, manifest["team"])
    for path in machos(stage):
        run("codesign", "--verify", "--strict", path)
    for name, value in manifest["artifacts"].items():
        require(digest(release / "artifacts" / name) == value, "Checksum mismatch: " + name)
    await_tickets(stage, args.ticket_timeout)
    manifest["status"] = "notarized"
    write_json(release / "manifest.json", manifest)
    print("Verified: " + str(release / "artifacts"))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--repo", type=Path, default=ROOT, help="checkout to release (default: this one)")
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("doctor")
    build_parser = commands.add_parser("build")
    build_parser.add_argument("--output", type=Path, required=True, help="new directory, outside the checkout")
    build_parser.add_argument("--allow-dirty", action="store_true", help="unsigned local candidate only")
    final_parser = commands.add_parser("finalize")
    final_parser.add_argument("--input", type=Path, required=True, help="a build's --output directory")
    final_parser.add_argument("--identity", required=True,
                              help="full 'Developer ID Application: NAME (TEAMID)' identity")
    final_parser.add_argument("--notary-profile", required=True, help="notarytool keychain profile name")
    verify_parser = commands.add_parser("verify")
    verify_parser.add_argument("--input", type=Path, required=True, help="a finalized build directory")
    for sub in (final_parser, verify_parser):
        sub.add_argument("--ticket-timeout", type=int, default=20,
                         help="minutes to wait for Gatekeeper to see the tickets (default 20)")
    args = parser.parse_args()
    {"doctor": lambda a: doctor(a.repo.resolve()), "build": build,
     "finalize": finalize, "verify": verify}[args.command](args)


if __name__ == "__main__":
    try:
        main()
    except (ReleaseError, subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError, KeyError) as error:
        print("Release stopped: " + str(error), file=sys.stderr)
        sys.exit(1)
