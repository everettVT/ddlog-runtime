"""Packaging contracts; no downloads or native compiler required."""
import hashlib
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('bootstrap', ROOT / 'scripts/bootstrap-native.py')
bootstrap = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bootstrap)


class NativePackaging(unittest.TestCase):
    def test_corrupt_archive_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            archive = Path(d) / 'archive'
            archive.write_bytes(b'original')
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            bootstrap.verify_archive(archive, digest)
            archive.write_bytes(b'tampered')
            with self.assertRaisesRegex(ValueError, 'remove .* and retry'):
                bootstrap.verify_archive(archive, digest)

    def test_operator_and_plain_locks_pin_identical_external_dependencies(self):
        locks = [tomllib.loads((ROOT / 'native' / name).read_text())['package']
                 for name in ('program.Cargo.lock', 'star.Cargo.lock')]
        external = [[p for p in packages if 'source' in p] for packages in locks]
        self.assertEqual(external[0], external[1])
        for package in external[0]:
            if package['source'].startswith('git+'):
                self.assertRegex(package['source'], r'#[0-9a-f]{40}$')
            else:
                self.assertRegex(package['checksum'], r'^[0-9a-f]{64}$')

    def test_driver_selects_lock_and_preserves_explicit_override(self):
        for star, override in ((False, False), (True, False), (True, True)):
            with self.subTest(star=star, override=override), tempfile.TemporaryDirectory() as d:
                root = Path(d)
                (root / 'bin').mkdir()
                compiler = root / 'bin/ddlog'
                compiler.write_text('#!/bin/sh\nmkdir -p program_ddlog/.cargo\n' +
                                    ('mkdir -p program_ddlog/types/lemmalog_star\n' if star else ''))
                compiler.chmod(0o755)
                cargo = root / 'cargo'
                cargo.write_text('#!/bin/sh\nprintf "%s\\n" "$@" > args\nmkdir -p target/debug\nprintf binary > target/debug/program_cli\n')
                cargo.chmod(0o755)
                (root / 'program.dl').write_text('fixture')
                (root / 'override.lock').write_text('explicit override')
                env = {k: v for k, v in os.environ.items() if not k.startswith(('DDLOG_', 'CARGO_'))}
                env.update(DDLOG_HOME=str(root), DDLOG_CARGO=str(cargo), DDLOG_OFFLINE='1',
                           DDLOG_LOCK_DIR=str(ROOT / 'native'))
                if override:
                    env['DDLOG_CARGO_LOCK'] = str(root / 'override.lock')
                subprocess.run([str(ROOT / 'scripts/build-ddlog.sh'), str(root / 'program.dl'), str(root / 'output')],
                               env=env, check=True, capture_output=True)
                expected = root / 'override.lock' if override else ROOT / 'native' / ('star.Cargo.lock' if star else 'program.Cargo.lock')
                self.assertEqual((root / 'program_ddlog/Cargo.lock').read_bytes(), expected.read_bytes())
                self.assertIn('--locked', (root / 'program_ddlog/args').read_text())
                self.assertEqual((root / 'output').read_text(), 'binary')


if __name__ == '__main__':
    unittest.main()
