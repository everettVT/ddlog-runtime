"""Content isolation unit tests and opt-in, real DDlog compile/query regression.

Native: source the supported toolchain env, then set DDLOG_NATIVE_ISOLATION=1.
DDLOG_NATIVE_TEST_ROOT may name a fresh private directory to retain logs/evidence.
No owner, existing world, HTTP server or provider is used.
"""
from contextlib import chdir
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / 'scripts/build-native-artifact.py'
spec = importlib.util.spec_from_file_location('native_artifact', HELPER)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class ArtifactOwnership(unittest.TestCase):
    def test_discovery_is_bounded_to_generated_project_and_cargo_configuration(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            project = root / 'project'
            (project / '.cargo').mkdir(parents=True)
            (project / '.cargo/config.toml').write_text('[build]\nrustc="fixture-rustc"\n')
            commands = []
            def capture(command, env):
                commands.append(command)
                self.assertEqual(command[1:], ['--version', '--verbose'])
                return 'fixture-version'
            env = {'CARGO_HOME': str(root / 'cargo-home'), 'PATH': '/fixture'}
            with patch.object(module, 'capture', capture), \
                    patch.object(module, 'executable', side_effect=lambda name, env: str(root / name)), \
                    patch.object(module.sys, 'platform', 'linux'):
                roots, versions = module.input_roots(['cargo', 'build', '--locked', '--offline'], env, project)
            self.assertIn(project, roots)
            self.assertTrue(all(p == project or p.name in ('config', 'config.toml') for p in roots))
            self.assertEqual(len(commands), 2)  # No cargo metadata or sysroot/vendor/SDK traversal.
            self.assertEqual(versions['rustc_path'], str(root / 'fixture-rustc'))
            self.assertEqual(versions['rustc'], 'fixture-version')

    def test_key_covers_project_sources_lock_configuration_and_relevant_flags_only(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = ['program/src/lib.rs', 'program/types/child/lib.rs', 'program/Cargo.lock',
                     'program/.cargo/config.toml', 'cargo-home/config.toml']
            for name in paths:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('original')
            roots = {root / 'program', root / 'cargo-home/config.toml'}
            env = {'RUSTFLAGS': '-C opt-level=0', 'PRIVATE_CREDENTIAL': 'must-not-be-recorded'}
            def fingerprint():
                return module.fingerprint(roots, {'rustc': 'fixture'}, ['cargo', 'build'], env, ())
            original, inputs = fingerprint()
            self.assertNotIn('must-not-be-recorded', json.dumps(inputs))
            for name in paths:
                with self.subTest(input=name):
                    path = root / name
                    before = path.stat()
                    path.write_text('modified')
                    os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
                    self.assertNotEqual(fingerprint()[0], original)
                    path.write_text('original')
                    self.assertEqual(fingerprint()[0], original)
            env.update(PRIVATE_CREDENTIAL='rotated-secret', UNRELATED_WORLD_GENERATION='2')
            self.assertEqual(fingerprint()[0], original)
            env['RUSTFLAGS'] = '-C opt-level=1'
            self.assertNotEqual(fingerprint()[0], original)
            env['RUSTFLAGS'] = '-C opt-level=0'
            self.assertNotEqual(module.fingerprint(roots, {'rustc': 'changed'},
                                ['cargo', 'build'], env, ())[0], original)
            self.assertNotEqual(module.fingerprint(roots, {'rustc': 'fixture'},
                                ['cargo', 'build', '--release'], env, ())[0], original)
            (root / 'program/target').mkdir()
            (root / 'program/target/ignored-output').write_text('not an input')
            self.assertEqual(fingerprint()[0], original)
            # A different generated project's equal package names never share artifacts.
            import shutil
            shutil.copytree(root / 'program', root / 'other')
            self.assertNotEqual(module.fingerprint({root / 'other', root / 'cargo-home/config.toml'},
                                {'rustc': 'fixture'}, ['cargo', 'build'], env, ())[0], original)

    def test_project_symlink_aliases_do_not_recurse_forever(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'dependency').write_text('first')
            (root / 'project').mkdir()
            (root / 'project/link').symlink_to(root / 'dependency')
            first = module.digest_tree(root / 'project')
            (root / 'dependency').write_text('other')
            self.assertNotEqual(module.digest_tree(root / 'project'), first)
            (root / 'project/cycle').symlink_to(root / 'project', target_is_directory=True)
            cyclic = module.digest_tree(root / 'project')
            self.assertEqual(module.digest_tree(root / 'project'), cyclic)

    def fixture(self, root):
        project = root / 'project'
        project.mkdir()
        (project / 'input.rs').write_text('alpha')
        compiler = root / 'compiler.py'
        compiler.write_text('''import json,os,sys,time
from pathlib import Path
source=Path('input.rs'); value=source.read_text()
target=Path(os.environ['CARGO_TARGET_DIR'])
assert str(target)==sys.argv[sys.argv.index('--target-dir')+1]
target.mkdir(parents=True,exist_ok=True)
marker=target/'active'
fd=os.open(marker,os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600)
try:
    artifact=target/'debug/program_cli'; artifact.parent.mkdir(exist_ok=True)
    artifact.write_text(value)
    time.sleep(.05)
    if value=='mutate': source.write_text('changed')
    if value=='failure': sys.exit(9)
    if value!='missing':
        print(json.dumps({'reason':'compiler-artifact','target':{'name':'program_cli','kind':['bin']},'executable':str(artifact)}))
finally:
    os.close(fd); marker.unlink()
''')
        return project, [sys.executable, str(compiler)]

    def test_exact_artifact_failure_and_changed_inputs_never_publish_stale_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project, command = self.fixture(root)
            base, output = root / 'cache', root / 'output'
            with chdir(project), patch.dict(os.environ, {'CARGO_TARGET_DIR': str(base)}), \
                    patch.object(module, 'input_roots', return_value=({project}, {})):
                self.assertEqual(module.build(output, command), 0)
                self.assertEqual(output.read_text(), 'alpha')
                for value in ('beta', 'alpha'):
                    (project / 'input.rs').write_text(value)
                    self.assertEqual(module.build(output, command), 0)
                    self.assertEqual(output.read_text(), value)
                self.assertEqual(len(list((base / 'ddlog-content-v2').iterdir())), 2)
                (project / 'input.rs').write_text('failure')
                self.assertEqual(module.build(output, command), 9)
                self.assertEqual(output.read_text(), 'alpha')
                (project / 'input.rs').write_text('missing')
                with self.assertRaisesRegex(ValueError, 'refusing stale artifact'):
                    module.build(output, command)
                (project / 'input.rs').write_text('mutate')
                with self.assertRaisesRegex(ValueError, 'quarantined'):
                    module.build(output, command)
                self.assertEqual(output.read_text(), 'alpha')
                self.assertEqual(len(list(base.glob('ddlog-content-v2/*/changed-inputs-*'))), 1)

    def test_key_lock_covers_atomic_output_publication(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project, command = self.fixture(root)
            base = root / 'cache'
            original = module.shutil.copy2
            def check_copy(source, destination):
                lock = module.project_lock(base, project.resolve())
                probe = subprocess.run([sys.executable, '-c', '''import fcntl,sys
with open(sys.argv[1],'r') as lock:
    try: fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
    except BlockingIOError: sys.exit(7)
''', str(lock)], timeout=5)
                self.assertEqual(probe.returncode, 7)
                self.assertEqual((root / 'output').read_text(), 'old')
                return original(source, destination)
            (root / 'output').write_text('old')
            with chdir(project), patch.dict(os.environ, {'CARGO_TARGET_DIR': str(base)}), \
                    patch.object(module, 'input_roots', return_value=({project}, {})), \
                    patch.object(module.shutil, 'copy2', check_copy):
                self.assertEqual(module.build(root / 'output', command), 0)
            self.assertEqual((root / 'output').read_text(), 'alpha')

    def test_concurrent_identical_inputs_share_key_and_keep_lock_through_copy(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project, command = self.fixture(root)
            # Mock discovery only; both separate helper processes use the real
            # fingerprint, locking, artifact parsing and publication code.
            bootstrap = '''import importlib.util,sys
from pathlib import Path
s=importlib.util.spec_from_file_location('artifact',sys.argv[1]);m=importlib.util.module_from_spec(s);s.loader.exec_module(m)
m.input_roots=lambda *args: ({Path.cwd()}, {})
sys.exit(m.build(sys.argv[2],sys.argv[3:]))
'''
            env = dict(os.environ, CARGO_TARGET_DIR=str(root / 'cache'))
            children = [subprocess.Popen([sys.executable, '-c', bootstrap, str(HELPER),
                        str(root / name), *command], cwd=project, env=env,
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE) for name in ('one', 'two')]
            try:
                for child in children:
                    stdout, stderr = child.communicate(timeout=15)
                    self.assertEqual(child.returncode, 0, (stdout + stderr).decode())
            finally:
                for child in children:
                    if child.poll() is None:
                        child.kill()
                        child.wait()
            self.assertEqual((root / 'one').read_text(), 'alpha')
            self.assertEqual((root / 'two').read_text(), 'alpha')
            self.assertEqual(len(list((root / 'cache/ddlog-content-v2').iterdir())), 1)


@unittest.skipUnless(os.environ.get('DDLOG_NATIVE_ISOLATION') == '1', 'opt-in native DDlog toolchain')
class NativeIsolation(unittest.TestCase):
    def test_simultaneous_and_sequential_programs_query_their_own_answers(self):
        retained = os.environ.get('DDLOG_NATIVE_TEST_ROOT')
        temporary = None
        if retained:
            root = Path(retained).resolve()
            root.mkdir(parents=True, exist_ok=False)
        else:
            temporary = tempfile.TemporaryDirectory(prefix='ddlog-native-isolation-')
            self.addCleanup(temporary.cleanup)
            root = Path(temporary.name)
        env = dict(os.environ, CARGO_TARGET_DIR=str(root / 'native-target'),
                   CARGO_HOME=str(root / 'cargo-home'))
        (root / 'cargo-home').mkdir()
        programs = [('alpha', 'Alpha', 7), ('beta', 'Beta', 107)]
        for name, relation, answer in programs:
            project = root / name
            project.mkdir()
            (project / 'program.dl').write_text(
                'input relation Seed(value: signed<64>)\n'
                f'output relation {relation}(value: signed<64>)\n'
                f'{relation}(v) :- Seed(x), var v = x + {answer - 7}.\n')
        # Both sources predate either native Cargo build, reproducing the stale
        # freshness condition. The two generated packages have identical names.
        builds = []
        try:
            for name, _, _ in programs:
                project = root / name
                log = (root / f'{name}-concurrent.log').open('w')
                child = subprocess.Popen([str(ROOT / 'scripts/build-ddlog.sh'),
                    str(project / 'program.dl'), str(project / 'program_cli')], env=env, stdout=log, stderr=log)
                builds.append((child, log))
            for child, log in builds:
                self.assertEqual(child.wait(timeout=600), 0, f'See retained log {log.name}')
        finally:
            for child, log in builds:
                if child.poll() is None:
                    child.kill()
                    child.wait()
                log.close()
        evidence = []
        def query(name, relation, answer, phase):
            binary = root / name / 'program_cli'
            result = subprocess.run([str(binary)], input=f'start;\ninsert Seed(7);\ncommit;\ndump {relation};\nexit;\n',
                                    text=True, capture_output=True, timeout=20)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(f'{relation}{{.value = {answer}}}', result.stdout)
            self.assertNotIn('Unknown', result.stdout + result.stderr)
            evidence.append({'program': name, 'phase': phase, 'stdout': result.stdout,
                             'stderr': result.stderr, 'source_sha256': module.digest_file(root / name / 'program.dl'),
                             'binary_sha256': module.digest_file(binary)})
        for name, relation, answer in programs:
            query(name, relation, answer, 'concurrent')
        self.assertNotEqual(evidence[0]['binary_sha256'], evidence[1]['binary_sha256'])
        # Interleave warm builds in reverse order; neither may inherit its neighbor.
        for name, relation, answer in reversed(programs):
            project = root / name
            with (root / f'{name}-sequential.log').open('w') as log:
                result = subprocess.run([str(ROOT / 'scripts/build-ddlog.sh'),
                    str(project / 'program.dl'), str(project / 'program_cli')], env=env,
                    stdout=log, stderr=log, timeout=600)
            self.assertEqual(result.returncode, 0, f'See {log.name}')
            query(name, relation, answer, 'sequential')
        buckets = list((root / 'native-target/ddlog-content-v2').iterdir())
        self.assertEqual(len(buckets), 2, 'Identical repeated build inputs must reuse their content namespace')
        (root / 'evidence.json').write_text(json.dumps({'passed': True, 'queries': evidence,
            'content_keys': sorted(p.name for p in buckets)}, indent=2) + '\n')
        print(f'Native isolation evidence: {root}', flush=True)


if __name__ == '__main__':
    unittest.main()
