#!/usr/bin/env python3
"""Prevent identical DDlog Cargo package names from sharing generated artifacts.

Cargo 1.65 may treat a different generated workspace as fresh in a shared target.
Each absolute project identity and generated-source/config/lock content key gets
its own target, including dependencies. Cargo's reported executable is the only
publication source; an old debug/program_cli is never a fallback.

This is a generated-project isolation boundary, not a hermetic build system.
External locked dependencies and the operator's pinned toolchain use Cargo's
normal validation/freshness rules. We do not traverse vendor trees, SDKs or Rust
sysroots, or include unrelated environment values (such as credentials) in keys.
A project lock covers two bounded source fingerprints, compilation and copy.
"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
import uuid


VERSION = 2


def digest_file(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def digest_tree(root, excluded=()):
    """Hash names, symlink targets, executable bits and all file contents."""
    excluded = {Path(p).resolve() for p in excluded}
    digest = hashlib.sha256()
    directories, files = set(), {}

    def add(value):
        data = json.dumps(value, ensure_ascii=True, separators=(',', ':')).encode()
        digest.update(len(data).to_bytes(8, 'big'))
        digest.update(data)

    def visit(path, relative):
        if path.is_symlink():
            add(['link', relative, os.readlink(path)])
            if not path.exists():
                add(['dangling', relative])
                return
        resolved = path.resolve(strict=True)
        if resolved in excluded:
            return
        if path.is_dir():
            if resolved in directories:
                # Aliases and parent links can otherwise expand exponentially.
                # Hash every physical tree once, retaining all alias identities.
                add(['directory-reference', relative, str(resolved)])
                return
            directories.add(resolved)
            add(['directory', relative])
            for child in sorted(path.iterdir()):
                # Cargo/libtool output and VCS administration are not sources.
                if child.name not in ('target', '.git'):
                    visit(child, relative + '/' + child.name)
        elif path.is_file():
            if resolved not in files:
                files[resolved] = digest_file(path)
            add(['file', relative, path.stat().st_mode & 0o111, files[resolved]])
        else:
            raise ValueError(f'Unsupported native input: {path}')

    root = Path(root)
    if not root.exists() and not root.is_symlink():
        add(['missing'])  # Also fence creation of previously absent Cargo configs.
    else:
        visit(root, '.')
    return digest.hexdigest()


def capture(command, env):
    result = subprocess.run(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    if result.returncode:
        raise ValueError(f'Native input discovery failed ({command[0]}): {result.stderr.strip()}')
    return result.stdout.strip()


def executable(name, env):
    found = shutil.which(name, path=env.get('PATH'))
    if found is None:
        raise ValueError(f'Native build tool is unavailable: {name}')
    return str(Path(found).resolve())


def input_roots(command, env, project):
    """Generated workspace, effective Cargo config and declared tool identities."""
    roots = {project}
    config_paths = [parent / '.cargo' / name for parent in (project, *project.parents)
                    for name in ('config', 'config.toml')]
    cargo_home = Path(env.get('CARGO_HOME', Path.home() / '.cargo'))
    config_paths += [cargo_home / name for name in ('config', 'config.toml')]
    roots.update(config_paths)
    rustc = env.get('RUSTC') or env.get('CARGO_BUILD_RUSTC')
    for path in config_paths:
        if path.is_file():
            rustc = rustc or tomllib.loads(path.read_text()).get('build', {}).get('rustc')
    cargo, rustc = executable(command[0], env), executable(rustc or 'rustc', env)
    versions = {'cargo_path': cargo, 'cargo': capture([cargo, '--version', '--verbose'], env),
                'rustc_path': rustc, 'rustc': capture([rustc, '--version', '--verbose'], env),
                'platform': sys.platform, 'machine': os.uname().machine}
    # Resolving the default Apple SDK is cheap; hashing its whole tree is not.
    if sys.platform == 'darwin':
        versions['sdk_path'] = env.get('SDKROOT') or capture(['xcrun', '--show-sdk-path'], env)
    return roots, versions


def build_environment(env):
    """Only supported code-generation/link configuration, never the whole env."""
    names = {'RUSTC', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER', 'RUSTFLAGS',
             'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_TARGET', 'CARGO_BUILD_RUSTC',
             'CARGO_BUILD_RUSTFLAGS', 'CC', 'CXX', 'AR', 'LD', 'RANLIB',
             'CFLAGS', 'CXXFLAGS', 'CPPFLAGS', 'LDFLAGS', 'SDKROOT',
             'MACOSX_DEPLOYMENT_TARGET', 'DEVELOPER_DIR', 'LIBRARY_PATH',
             'CPATH', 'CPLUS_INCLUDE_PATH', 'PKG_CONFIG_PATH'}
    return {name: value for name, value in env.items() if name in names
            or name.startswith('CARGO_PROFILE_')
            or (name.startswith('CARGO_TARGET_') and name.endswith(('_LINKER', '_RUSTFLAGS')))}


def fingerprint(roots, versions, command, env, excluded):
    started = time.perf_counter()
    # Even supported build flags can contain private values; persist only a hash.
    environment = hashlib.sha256(json.dumps(build_environment(env), sort_keys=True).encode()).hexdigest()
    inputs = {'schema_version': VERSION, 'command': command, 'build_environment_sha256': environment,
              'tool_versions': versions,
              'inputs': {str(p): digest_tree(p, excluded) for p in sorted(roots)}}
    key = hashlib.sha256(json.dumps(inputs, sort_keys=True).encode()).hexdigest()
    print(f'generated-project fingerprint: {time.perf_counter() - started:.3f}s', flush=True)
    return key, inputs


def project_lock(base, project):
    return base / 'ddlog-project-locks' / (hashlib.sha256(str(project).encode()).hexdigest() + '.lock')


def cargo_build(command, env, target):
    artifacts = set()
    invocation = [*command, '--target-dir', str(target), '--message-format=json-render-diagnostics']
    with subprocess.Popen(invocation, env=env, stdout=subprocess.PIPE, text=True) as process:
        for line in process.stdout:
            try:
                event = json.loads(line)
            except ValueError:
                print(line, end='', flush=True)
                continue
            if (event.get('reason') == 'compiler-artifact' and event.get('target', {}).get('name') == 'program_cli'
                    and 'bin' in event['target'].get('kind', []) and event.get('executable')):
                artifacts.add(Path(event['executable']).resolve())
        status = process.wait()
    if status:
        return status, None
    if len(artifacts) != 1:
        raise ValueError('Cargo did not report exactly one program_cli executable; refusing stale artifact fallback')
    artifact = artifacts.pop()
    if target.resolve() not in artifact.parents or not artifact.is_file():
        raise ValueError('Cargo executable is outside the isolated target or missing')
    return 0, artifact


def publish(artifact, output):
    descriptor, staging = tempfile.mkstemp(prefix='.' + output.name + '-', dir=output.parent)
    os.close(descriptor)
    try:
        shutil.copy2(artifact, staging)
        with open(staging, 'rb') as stream:
            os.fsync(stream.fileno())
        os.replace(staging, output)
    finally:
        if os.path.exists(staging):
            os.unlink(staging)


def build(output, command):
    project = Path.cwd().resolve()
    output = Path(output).absolute()
    base = Path(os.environ.get('CARGO_TARGET_DIR', 'target')).resolve()
    cache = base / 'ddlog-content-v2'
    cache.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    lock_path = project_lock(base, project)
    lock_path.parent.mkdir(exist_ok=True)
    lock = os.open(lock_path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(lock, fcntl.LOCK_EX)
        # Read only after the project lock; no redundant pre-lock fingerprint.
        roots, versions = input_roots(command, env, project)
        key, inputs = fingerprint(roots, versions, command, env, (base, output))
        bucket = cache / key
        bucket.mkdir(exist_ok=True)
        (bucket / 'inputs.json').write_text(json.dumps(inputs, sort_keys=True, indent=2) + '\n')
        target = bucket / 'target'
        build_env = dict(env, CARGO_TARGET_DIR=str(target))
        print(f'ddlog-runtime native content key: {key}\ntarget: {target}', flush=True)
        status, artifact = cargo_build(command, build_env, target)
        if status:
            return status
        if fingerprint(roots, versions, command, env, (base, output))[0] != key:
            target.rename(bucket / ('changed-inputs-' + uuid.uuid4().hex))
            raise ValueError('Generated project inputs changed during compilation; artifact quarantined, not published')
        publish(artifact, output)
        print(f'published native sha256: {digest_file(output)}', flush=True)
        return 0
    finally:
        os.close(lock)


if __name__ == '__main__':
    try:
        raise SystemExit(build(sys.argv[1], sys.argv[2:]))
    except (ValueError, OSError, KeyError) as error:
        print(f'build-native-artifact.py: {error}', file=sys.stderr)
        raise SystemExit(1)
