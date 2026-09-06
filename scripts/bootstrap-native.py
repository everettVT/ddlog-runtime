#!/usr/bin/env python3
"""Install the pinned DDlog compiler, native Rust and complete offline dependencies."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import subprocess
import tarfile

REPO = Path(__file__).resolve().parent.parent
RELEASES = {
    'Darwin': ('ddlog-v1.2.3-20211213235114-macOS.tar.gz',
               '65d3613bae6ff959e6159a4edf7c7f4eda1634defecb144dd0d9e71291e497a9'),
    'Linux': ('ddlog-v1.2.3-20211213235218-Linux.tar.gz',
              'a54eb4bc8bfce926e5391a9c03e0b07477de0c26799c05b6e381436803104115'),
}


def verify_archive(path, expected):
    actual = hashlib.sha256(path.read_bytes()).hexdigest()
    if actual != expected:
        raise ValueError(f'Archive checksum mismatch: {actual}; remove {path} and retry')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path, help='New, dedicated installation directory')
    args = parser.parse_args()
    system, machine = platform.system(), platform.machine()
    if system not in RELEASES or (system == 'Linux' and machine != 'x86_64'):
        parser.error('Supported release platforms: macOS and Linux x86_64')
    root = args.directory.expanduser().resolve()
    if root.exists():
        parser.error('Use a new directory; incomplete installations are retained for diagnosis')
    for tool in ('rustup', 'cargo', 'curl', 'cc'):
        if not shutil.which(tool):
            parser.error(f'Install {tool} first; see docs/building.md')
    rustup = shutil.which('rustup')
    cargo = subprocess.check_output([rustup, 'which', 'cargo'], text=True).strip()
    root.mkdir(parents=True)
    asset, checksum = RELEASES[system]
    archive = root / asset
    subprocess.run(['curl', '--fail', '--location', '--retry', '3', '--output', str(archive),
                    'https://github.com/vmware-archive/differential-datalog/releases/download/v1.2.3/' + asset], check=True)
    verify_archive(archive, checksum)
    with tarfile.open(archive) as bundle:
        bundle.extractall(root, filter='data')
    env = dict(os.environ)
    for key in ('RUSTC', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER', 'RUSTUP_TOOLCHAIN',
                'CARGO_TARGET_DIR', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTFLAGS'):
        env.pop(key, None)
    env.update(DDLOG_HOME=str(root / 'ddlog'), CARGO_HOME=str(root / 'cargo-home'),
               RUSTUP_HOME=str(root / 'rustup'))
    subprocess.run([rustup, 'toolchain', 'install', '1.65.0', '--profile', 'minimal', '--no-self-update'], env=env, check=True)
    native = {tool: subprocess.check_output([rustup, 'which', '--toolchain', '1.65.0', tool], env=env, text=True).strip()
              for tool in ('cargo', 'rustc')}
    probe = root / 'probe'
    probe.mkdir()
    (probe / 'program.dl').write_text('input relation Input(x: signed<64>)\noutput relation Output(x: signed<64>)\nOutput(x) :- Input(x).\n')
    subprocess.run([str(root / 'ddlog/bin/ddlog'), '-i', 'program.dl'], cwd=probe, env=env, check=True)
    project = probe / 'program_ddlog'
    shutil.copyfile(REPO / 'native/program.Cargo.lock', project / 'Cargo.lock')
    # Modern Cargo acquires dependencies; native compilation uses Rust 1.65.
    with (root / 'vendor-config.toml').open('w') as config:
        subprocess.run([cargo, 'vendor', '--locked', str(root / 'vendor')], cwd=project, env=env, stdout=config, check=True)
    shutil.copytree(REPO / 'native', root / 'locks')
    exports = dict(DDLOG_HOME=str(root / 'ddlog'), DDLOG_CARGO=native['cargo'],
                   RUSTC=native['rustc'], CARGO_HOME=str(root / 'cargo-home'),
                   CARGO_TARGET_DIR=str(root / 'target'),
                   DDLOG_CARGO_CONFIG=str(root / 'vendor-config.toml'),
                   DDLOG_LOCK_DIR=str(root / 'locks'), DDLOG_OFFLINE='1',
                   LEMMALOG_DDLOG_BUILD=str(REPO / 'scripts/build-ddlog.sh'))
    (root / 'env.sh').write_text('unset DDLOG_CARGO_LOCK\n' + ''.join(f'export {k}={shlex.quote(v)}\n' for k, v in exports.items()))
    (root / 'receipt.json').write_text(json.dumps({'compiler_archive': asset, 'compiler_sha256': checksum,
        'rust': '1.65.0', 'platform': [system, machine],
        'lock_sha256': {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in (root / 'locks').glob('*.lock')}}, indent=2) + '\n')
    print(f'Installed. For native builds only: . {shlex.quote(str(root / "env.sh"))}')


if __name__ == '__main__':
    main()
