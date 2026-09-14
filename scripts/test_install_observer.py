import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('install_observer', Path(__file__).with_name('install-observer.py'))
hook = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hook)

class InstallObserverTests(unittest.TestCase):
    def test_patch_is_exact_and_idempotent(self):
        for original, patched in [(hook.WORKER_SITE, hook.WORKER_PATCH), (hook.CLI_SITE, hook.CLI_PATCH)]:
            source = 'prefix\n' + original + '\nsuffix'
            result = hook.patch_once(source, original, patched, 'test')
            self.assertEqual(result, 'prefix\n' + patched + '\nsuffix')
            self.assertEqual(hook.patch_once(result, original, patched, 'test'), result)
            for invalid in ['', original + original, patched + patched, patched + original]:
                with self.assertRaises(ValueError):
                    hook.patch_once(invalid, original, patched, 'test')

    def test_failed_validation_does_not_partially_patch(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths = {'differential_datalog/src/lib.rs':'library',
                     'differential_datalog/src/program/worker.rs':hook.WORKER_SITE,
                     'src/main.rs':'changed CLI'}
            for name, content in paths.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content)
            with self.assertRaises(ValueError):
                hook.install(root)
            for name, content in paths.items():
                self.assertEqual((root/name).read_text(),content)
            self.assertFalse((root/'differential_datalog/src/observer.rs').exists())
            (root/'src/main.rs').write_text(hook.CLI_SITE)
            hook.install(root)
            first = {str(p.relative_to(root)):p.read_bytes() for p in root.rglob('*.rs')}
            hook.install(root)
            second = {str(p.relative_to(root)):p.read_bytes() for p in root.rglob('*.rs')}
            self.assertEqual(first,second)

if __name__ == '__main__': unittest.main()
